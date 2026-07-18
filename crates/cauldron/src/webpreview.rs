//! WebStorm-style live web preview — a tiny static HTTP server (std::net only, no deps) that
//! serves a project directory to the system browser and auto-reloads it when any file changes.
//!
//! Design: bind `127.0.0.1:0` (ephemeral port), serve files under `root` with `/` → index.html,
//! and inject a live-reload poller into every served HTML page. A background scanner bumps a
//! generation counter whenever a file under `root` changes mtime; the injected script polls
//! `/__cauldron_reload`, and reloads the page when the number moves. One server per app; opening
//! a different root restarts it. Localhost-only, read-only, path-traversal-guarded.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// The live-reload snippet injected before `</body>` of every served HTML page.
const RELOAD_JS: &str = r#"<script>
(function(){let last=null;setInterval(async function(){try{
let r=await fetch('/__cauldron_reload');let n=await r.text();
if(last!==null&&n!==last){location.reload();}last=n;}catch(e){}},1000);})();
</script>"#;

pub struct WebServer {
    root: PathBuf,
    port: u16,
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl WebServer {
    /// Root directory being served.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Base URL, e.g. `http://127.0.0.1:38191`.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Start a server rooted at `root`. Spawns the accept loop + the change scanner on
    /// background threads. Returns None if it can't bind (extremely rare on localhost).
    pub fn start(root: &Path) -> Option<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        let root = root.to_path_buf();
        let generation = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        // Accept loop.
        {
            let root = root.clone();
            let generation = Arc::clone(&generation);
            let stop = Arc::clone(&stop);
            let _ = listener.set_nonblocking(true);
            std::thread::Builder::new().name("cauldron-web".into()).spawn(move || {
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    match listener.accept() {
                        Ok((s, _)) => {
                            let root = root.clone();
                            let gen = generation.load(Ordering::Relaxed);
                            std::thread::spawn(move || handle(s, &root, gen));
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(40));
                        }
                        Err(_) => return,
                    }
                }
            });
        }
        // Change scanner: bump the generation when any file mtime under root moves.
        {
            let root = root.clone();
            let generation = Arc::clone(&generation);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new().name("cauldron-web-watch".into()).spawn(move || {
                let mut last = latest_mtime(&root);
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let now = latest_mtime(&root);
                    if now != last {
                        last = now;
                        generation.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
        Some(Self { root, port, generation, stop })
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Newest mtime (as secs) of any file under `root`, bounded so a giant tree can't stall the
/// scanner. Cheap change signal — exact set doesn't matter, only that SOMETHING moved.
fn latest_mtime(root: &Path) -> u64 {
    fn walk(dir: &Path, best: &mut u64, budget: &mut u32) {
        if *budget == 0 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name == ".git" || name == "node_modules" || name == "target" {
                    continue;
                }
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&path, best, budget);
            } else if let Ok(m) = meta.modified() {
                if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
                    *best = (*best).max(d.as_secs());
                }
            }
        }
    }
    let mut best = 0;
    let mut budget = 20_000u32;
    walk(root, &mut best, &mut budget);
    best
}

/// Serve one request. GET only; `/__cauldron_reload` returns the generation; everything else
/// maps to a file under `root` (path-traversal guarded), HTML gets the reload snippet injected.
fn handle(mut stream: TcpStream, root: &Path, generation: u64) {
    let mut buf = [0u8; 8192];
    let Ok(n) = stream.read(&mut buf) else { return };
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.split_whitespace().nth(1).unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");

    if path == "/__cauldron_reload" {
        let body = generation.to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
            body.len(),
            body
        );
        return;
    }

    // Resolve the target file, guarding against `..` traversal outside root.
    let rel = path.trim_start_matches('/');
    let decoded = percent_decode(rel);
    let mut target = root.join(&decoded);
    if decoded.is_empty() || path.ends_with('/') {
        target = target.join("index.html");
    }
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let ok = target
        .canonicalize()
        .map(|c| c.starts_with(&canon_root))
        .unwrap_or(false);
    if !ok {
        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return;
    }
    let Ok(bytes) = std::fs::read(&target) else {
        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    let ctype = content_type(&target);
    if ctype == "text/html" {
        // Inject the reload poller just before </body> (append if there is none).
        let mut html = String::from_utf8_lossy(&bytes).into_owned();
        match html.rfind("</body>") {
            Some(i) => html.insert_str(i, RELOAD_JS),
            None => html.push_str(RELOAD_JS),
        }
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n",
            html.len()
        );
        let _ = stream.write_all(html.as_bytes());
    } else {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n",
            ctype,
            bytes.len()
        );
        let _ = stream.write_all(&bytes);
    }
}

/// Minimal `%XX` percent-decoding for request paths (spaces, unicode filenames).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Content-Type from the file extension — the handful the browser actually needs sniffed.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_and_decode() {
        assert_eq!(content_type(Path::new("a.html")), "text/html");
        assert_eq!(content_type(Path::new("a.CSS")), "text/css");
        assert_eq!(content_type(Path::new("x.bin")), "application/octet-stream");
        assert_eq!(percent_decode("a%20b.html"), "a b.html");
        assert_eq!(percent_decode("plain.js"), "plain.js");
    }

    /// End-to-end: serve a temp dir, fetch index.html, confirm the reload snippet is injected
    /// and a CSS file serves with the right type.
    #[test]
    fn serves_files_with_reload_injection() {
        let dir = std::env::temp_dir().join(format!("cauldron-web-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<html><body><h1>hi</h1></body></html>").unwrap();
        std::fs::write(dir.join("style.css"), "body{color:red}").unwrap();
        let srv = WebServer::start(&dir).expect("bind");

        let get = |path: &str| -> String {
            let mut s = TcpStream::connect(format!("127.0.0.1:{}", srv.port)).unwrap();
            write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            out
        };
        let index = get("/");
        assert!(index.contains("<h1>hi</h1>"), "serves index.html at /");
        assert!(index.contains("__cauldron_reload"), "injects the reload poller");
        let css = get("/style.css");
        assert!(css.contains("text/css"));
        assert!(css.contains("body{color:red}"));
        // Traversal guard.
        let escape = get("/../../etc/passwd");
        assert!(escape.contains("404"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
