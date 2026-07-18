//! AI inline completion — Claude-powered ghost text.
//!
//! Cadence: the app calls [`AiCompleter::tick`] every frame with the focused file's caret
//! anchor `(byte, generation)`. When the anchor has been STABLE for the debounce window (the
//! user paused typing), one background request goes out (cider PTY template: thread + mpsc +
//! request_repaint). The reply is dropped unless the anchor is still identical — a moved caret
//! or any edit invalidates it, so stale ghosts can never appear.
//!
//! Auth: the SAME Claude Code OAuth sign-in the usage meter reads (`~/.claude/.credentials.json`)
//! — no separate login. The token stays in the request thread and is never logged or displayed.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::settings::{AiProvider, AiSettings};

/// The active backend config. Request workers run on their own threads long after the settings
/// dialog closed, so they read this snapshot instead of borrowing `App` — set at boot and
/// whenever the AI settings change.
static AI_CONFIG: Mutex<Option<AiSettings>> = Mutex::new(None);

pub(crate) fn set_config(cfg: &AiSettings) {
    *AI_CONFIG.lock().unwrap_or_else(|p| p.into_inner()) = Some(cfg.clone());
}

fn config() -> AiSettings {
    AI_CONFIG.lock().unwrap_or_else(|p| p.into_inner()).clone().unwrap_or_default()
}

/// Can the CONFIGURED backend answer right now? Claude → signed in; Ollama → server reachable.
/// The Ollama probe is a localhost round-trip (instant refusal when the daemon is down, 2s cap
/// otherwise) — call at boot and on settings changes, not per frame.
pub(crate) fn backend_available() -> bool {
    match config().provider {
        AiProvider::Claude => token().is_some(),
        AiProvider::Ollama => ollama_version(&config().ollama_url).is_some(),
    }
}

/// OAuth requirement — requests through the Claude Code sign-in must use exactly this system
/// string (the Ollama path ignores the constraint but shares the constant).
pub(crate) const OAUTH_SYSTEM: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Pause after the last caret/edit activity before a completion is requested. The local
/// backend gets a shorter fuse: no rate limits, no billing, and the whole point of running
/// on-box is completions that keep up with typing.
const DEBOUNCE: Duration = Duration::from_millis(250);
const DEBOUNCE_LOCAL: Duration = Duration::from_millis(120);

fn debounce() -> Duration {
    match config().provider {
        AiProvider::Claude => DEBOUNCE,
        AiProvider::Ollama => DEBOUNCE_LOCAL,
    }
}
/// Context sent around the caret (chars). Generous but bounded — this fires often.
const PREFIX_CHARS: usize = 6000;
const SUFFIX_CHARS: usize = 2000;
const MODEL: &str = "claude-haiku-4-5-20251001";

/// One completed request: ghost text for (path, byte, generation).
pub struct AiGhost {
    pub path: PathBuf,
    pub byte: usize,
    pub generation: u64,
    pub text: String,
}

pub struct AiCompleter {
    /// Master switch (status-bar chip toggles it; persisted in the session).
    pub enabled: bool,
    tx: Sender<AiGhost>,
    rx: Receiver<AiGhost>,
    /// Anchor being watched: (path, byte, generation, first-seen).
    watch: Option<(PathBuf, usize, u64, Instant)>,
    /// Last anchor actually requested (never re-request the same spot).
    requested: Option<(PathBuf, usize, u64)>,
    in_flight: bool,
    /// Configured backend usable? (Claude sign-in present / Ollama reachable.) Rechecked via
    /// [`Self::refresh_available`] when AI settings change; the chip shows "AI off" without it.
    pub available: bool,
}

impl AiCompleter {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let available = backend_available();
        if available {
            warm_local();
        }
        Self { enabled: available, tx, rx, watch: None, requested: None, in_flight: false, available }
    }

    /// Re-probe the configured backend (after a provider/model change in Settings). Turning a
    /// previously-unavailable backend on also flips `enabled` on — that's what the user came for.
    pub fn refresh_available(&mut self) {
        let was = self.available;
        self.available = backend_available();
        if self.available {
            warm_local();
        }
        if self.available && !was {
            self.enabled = true;
        }
    }

    /// Frame-driven state machine. `anchor` is `EditorView::ghost_anchor` for the focused file
    /// (None = selection/multi-caret/unfocused → reset). Fires at most one request at a time.
    pub fn tick(
        &mut self,
        anchor: Option<(PathBuf, usize, u64)>,
        rope_text: impl FnOnce() -> String,
        byte_in_text: usize,
        ctx: &egui::Context,
    ) {
        if !self.enabled || !self.available {
            self.watch = None;
            return;
        }
        let Some((path, byte, generation)) = anchor else {
            self.watch = None;
            return;
        };
        // (Re)arm the watch when the anchor moves.
        match &self.watch {
            Some((p, b, g, _)) if *p == path && *b == byte && *g == generation => {}
            _ => {
                self.watch = Some((path.clone(), byte, generation, Instant::now()));
                ctx.request_repaint_after(debounce());
                return;
            }
        }
        if self.in_flight
            || self.requested.as_ref().is_some_and(|(p, b, g)| *p == path && *b == byte && *g == generation)
        {
            return;
        }
        let armed_at = self.watch.as_ref().unwrap().3;
        let debounce = debounce();
        if armed_at.elapsed() < debounce {
            ctx.request_repaint_after(debounce - armed_at.elapsed());
            return;
        }

        // Fire.
        self.in_flight = true;
        self.requested = Some((path.clone(), byte, generation));
        let text = rope_text();
        let byte = byte_in_text.min(text.len());
        let prefix: String = text[..byte].chars().rev().take(PREFIX_CHARS).collect::<Vec<_>>().into_iter().rev().collect();
        let suffix: String = text[byte..].chars().take(SUFFIX_CHARS).collect();
        let lang_hint = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        // ALWAYS send, even on failure/empty (as empty text): pump() clears in_flight only when
        // a message arrives, so a success-only send left one failed request wedging the
        // completer permanently — no ghost ever again until restart.
        let spawned = std::thread::Builder::new()
            .name("cauldron-ai".into())
            .spawn(move || {
                let text = complete(&prefix, &suffix, &lang_hint).unwrap_or_default();
                let _ = tx.send(AiGhost { path, byte, generation, text });
                ctx2.request_repaint();
            });
        if spawned.is_err() {
            self.in_flight = false; // no worker → nothing will ever report back
        }
    }

    /// Drain finished completions (the app validates anchor freshness before installing).
    /// Empty-text results only exist to clear `in_flight`; they are not ghosts.
    pub fn pump(&mut self) -> Vec<AiGhost> {
        let out: Vec<AiGhost> = self.rx.try_iter().collect();
        if !out.is_empty() {
            self.in_flight = false;
        }
        out.into_iter().filter(|g| !g.text.is_empty()).collect()
    }

}

/// `(credentials mtime, parsed token)` memo: the file is re-read ONLY when its mtime changes
/// (sign-in / token refresh), not on every completion request or usage-meter cycle
/// (boot-wave item 5 — this is the single shared read; usage.rs calls here too).
static TOKEN_CACHE: std::sync::Mutex<Option<(std::time::SystemTime, Option<String>)>> =
    std::sync::Mutex::new(None);

pub(crate) fn token() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let creds = PathBuf::from(&home).join(".claude/.credentials.json");
    let mtime = std::fs::metadata(&creds).ok().and_then(|m| m.modified().ok());
    let mut cache = TOKEN_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let (Some(m), Some((cached_m, tok))) = (mtime, cache.as_ref()) {
        if *cached_m == m {
            return tok.clone();
        }
    }
    let tok = std::fs::read_to_string(&creds).ok().and_then(|t| parse_access_token(&t));
    // Only memoize when the file is stat-able; a vanished file drops the memo
    // so a re-created sign-in is picked up immediately.
    *cache = mtime.map(|m| (m, tok.clone()));
    tok
}

/// Extract the OAuth access token from a `~/.claude/.credentials.json` body (pure).
fn parse_access_token(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(v.get("claudeAiOauth")?.get("accessToken")?.as_str()?.to_string())
}

/// One fill-in-the-middle request. Returns the raw insertion text (already trimmed of fences).
fn complete(prefix: &str, suffix: &str, lang: &str) -> Option<String> {
    let cfg = config();
    if cfg.provider == AiProvider::Ollama {
        return ollama_fim(&cfg, prefix, suffix);
    }
    let user = format!(
        "Complete the code at the <CURSOR> marker in this {lang} file. Reply with ONLY \
         the text to insert at the cursor — no explanation, no markdown fences, no \
         repetition of existing code. At most 4 lines. If nothing useful can be \
         inserted, reply with an empty message.\n\n{prefix}<CURSOR>{suffix}"
    );
    // GRACEFUL DEGRADATION: a failed Claude request (credits exhausted, rate limit,
    // offline) falls straight through to the local model when Ollama is up — completions
    // keep flowing instead of going dark.
    let Some(raw) = ask("You are Claude Code, Anthropic's official CLI for Claude.", &user, MODEL, 160, Some(0.0)) else {
        return ollama_fim(&cfg, prefix, suffix);
    };
    let text = unfence(&raw);
    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

/// The reusable request core: one system+user turn against the CONFIGURED backend. `model` and
/// `temperature` apply to the Claude path; the Ollama path uses the configured chat model
/// (callers pick a Claude tier, not a local one). Returns the reply text, or None on any failure.
pub(crate) fn ask(
    system: &str,
    user: &str,
    model: &str,
    max_tokens: u32,
    temperature: Option<f32>,
) -> Option<String> {
    let cfg = config();
    if cfg.provider == AiProvider::Ollama {
        return ollama_chat(&cfg, system, user, max_tokens);
    }
    // GRACEFUL DEGRADATION: credits exhausted / rate-limited / offline → the local model
    // answers instead of the feature going dark. No-op when Ollama isn't running.
    ask_claude(system, user, model, max_tokens, temperature)
        .or_else(|| ollama_chat(&cfg, system, user, max_tokens))
}

/// Streaming [`ask`]: `on_delta` receives reply fragments as they arrive (Ollama streams
/// token-by-token; the Claude path is one-shot, so the whole reply lands as a single delta).
/// Returns the full reply, or None on failure.
pub(crate) fn ask_stream(
    system: &str,
    user: &str,
    model: &str,
    max_tokens: u32,
    temperature: Option<f32>,
    on_delta: &mut dyn FnMut(&str),
) -> Option<String> {
    let cfg = config();
    if cfg.provider == AiProvider::Ollama {
        let body = serde_json::json!({
            "model": cfg.ollama_chat_model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "stream": true,
            "keep_alive": "30m",
            "options": { "num_predict": max_tokens },
        });
        let mut acc = String::new();
        let ok = ollama_stream(&cfg.ollama_url, "/api/chat", &body, |frag| {
            acc.push_str(frag);
            on_delta(frag);
            false
        });
        if !ok || acc.is_empty() {
            return None;
        }
        return Some(acc);
    }
    match ask_claude(system, user, model, max_tokens, temperature) {
        Some(full) => {
            on_delta(&full);
            Some(full)
        }
        // Same degradation as ask(): stream the answer from the local model instead.
        None => {
            let body = serde_json::json!({
                "model": cfg.ollama_chat_model,
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user", "content": user },
                ],
                "stream": true,
                "keep_alive": "30m",
                "options": { "num_predict": max_tokens },
            });
            let mut acc = String::new();
            let ok = ollama_stream(&cfg.ollama_url, "/api/chat", &body, |frag| {
                acc.push_str(frag);
                on_delta(frag);
                false
            });
            (ok && !acc.is_empty()).then_some(acc)
        }
    }
}

/// One system+user turn against the Messages API via the same Claude Code OAuth token.
/// Returns the first text block, or None on any failure.
///
/// Token hygiene is identical to `complete`'s original: the bearer token travels via
/// `curl --config -` on STDIN — never argv, never logged, never written to disk.
fn ask_claude(
    system: &str,
    user: &str,
    model: &str,
    max_tokens: u32,
    temperature: Option<f32>,
) -> Option<String> {
    let tok = token()?;
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": [{ "role": "user", "content": user }],
    });
    // Claude 5 models REJECT the temperature param ("deprecated for this model") — only send
    // it where the caller wants it and the model tier accepts it (haiku ghost completions).
    if let Some(t) = temperature {
        body["temperature"] = serde_json::json!(t);
    }
    // The token travels via `curl --config -` on STDIN, never argv (argv is world-readable in
    // /proc). The request body carries the user's CODE (prefix/suffix around the caret) — the
    // XDG_RUNTIME_DIR path is mode-700, but the /tmp fallback is world-readable by default, so
    // the file is created 0600 explicitly regardless of directory.
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    static REQ_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = REQ_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let body_path = dir.join(format!("cauldron-ai-{}-{}.json", std::process::id(), seq));
    write_private(&body_path, &serde_json::to_vec(&body).ok()?)?;
    let cfg = format!(
        concat!(
            "url = \"https://api.anthropic.com/v1/messages\"\n",
            "header = \"Authorization: Bearer {}\"\n",
            "header = \"anthropic-version: 2023-06-01\"\n",
            "header = \"anthropic-beta: oauth-2025-04-20\"\n",
            "header = \"content-type: application/json\"\n",
            "data-binary = \"@{}\"\n",
        ),
        tok,
        body_path.display()
    );
    // Timeout scales with the answer size: ghost text keeps its original snappy 15s
    // (a hung request pins the completer's in_flight for the whole ceiling), while
    // big-answer action requests (1500 tokens) get 90s.
    let timeout = if max_tokens <= 256 { "15" } else { "90" };
    let mut child = std::process::Command::new("curl")
        .args(["-sS", "-m", timeout, "--config", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    use std::io::Write as _;
    // Never early-return between spawn and wait: that would leak a zombie curl and the
    // body temp file. Record the write outcome and always reap + clean up.
    let wrote = child
        .stdin
        .take()
        .map(|mut s| s.write_all(cfg.as_bytes()).is_ok())
        .unwrap_or(false);
    let out = child.wait_with_output().ok();
    let _ = std::fs::remove_file(&body_path);
    let out = out?;
    if !wrote || !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    if let Some(err) = v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
        log::warn!("claude api error: {err}");
        return None;
    }
    // First TEXT block — models with reasoning may emit a thinking block first.
    let raw = v
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|b| b.get("text").and_then(|t| t.as_str()))?;
    Some(raw.to_string())
}

/// POST `body` to the local Ollama server, JSON in / JSON out. Body arrives via stdin — no
/// temp file needed, nothing here is secret. None on any failure (server down, model missing,
/// timeout); Ollama reports errors as `{"error": …}` with a 200-ish transport, so check both.
fn ollama_post(base: &str, path: &str, body: &serde_json::Value, timeout_s: &str) -> Option<serde_json::Value> {
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let payload = serde_json::to_vec(body).ok()?;
    let mut child = std::process::Command::new("curl")
        .args(["-sS", "-m", timeout_s, "-X", "POST", "--data-binary", "@-", &url])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    use std::io::Write as _;
    let wrote = child
        .stdin
        .take()
        .map(|mut s| s.write_all(&payload).is_ok())
        .unwrap_or(false);
    let out = child.wait_with_output().ok()?;
    if !wrote || !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        log::warn!("ollama error: {err}");
        return None;
    }
    Some(v)
}

/// Preload the local FIM model (background thread, fire-and-forget) so the FIRST ghost after
/// boot or a provider switch doesn't pay the multi-second model-load cost on top of inference.
/// `keep_alive` then holds it resident between completions. No-op on the Claude provider.
pub(crate) fn warm_local() {
    let cfg = config();
    if cfg.provider != AiProvider::Ollama {
        return;
    }
    let _ = std::thread::Builder::new().name("cauldron-ai-warm".into()).spawn(move || {
        let body = serde_json::json!({
            "model": cfg.ollama_fim_model,
            "prompt": "",
            "stream": false,
            "keep_alive": "30m",
            "options": { "num_predict": 0 },
        });
        let _ = ollama_post(&cfg.ollama_url, "/api/generate", &body, "120");
    });
}

/// Native FIM against the local server: Ollama applies the model's fill-in-the-middle
/// template when `suffix` is present (qwen2.5-coder base, codellama:code, starcoder2, …).
/// STREAMED, and cut off early: the ghost shows at most 4 lines, but a base model happily
/// generates 128 tokens of babble past the cursor — killing the request the moment we have
/// enough is what makes local completions feel instant.
fn ollama_fim(cfg: &AiSettings, prefix: &str, suffix: &str) -> Option<String> {
    let body = serde_json::json!({
        "model": cfg.ollama_fim_model,
        "prompt": prefix,
        "suffix": suffix,
        "stream": true,
        "keep_alive": "30m",
        "options": { "num_predict": 128, "temperature": 0.0 },
    });
    let raw = ollama_generate_stream(&cfg.ollama_url, &body, |acc| {
        // Enough once a 5th line starts (we keep 4) or the model re-emits the code
        // after the cursor — the same conditions trim_fim cuts at.
        let stop = suffix.lines().find(|l| !l.trim().is_empty()).map(str::trim);
        acc.lines().count() > 4 || acc.lines().any(|l| stop.is_some_and(|s| l.trim() == s))
    })?;
    trim_fim(&raw, suffix)
}

/// One-shot chat against the local server (the Ollama provider path AND the fallback when
/// a Claude request fails).
fn ollama_chat(cfg: &AiSettings, system: &str, user: &str, max_tokens: u32) -> Option<String> {
    let body = serde_json::json!({
        "model": cfg.ollama_chat_model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
        "stream": false,
        "keep_alive": "30m",
        "options": { "num_predict": max_tokens },
    });
    let v = ollama_post(&cfg.ollama_url, "/api/chat", &body, "120")?;
    Some(v.get("message")?.get("content")?.as_str()?.to_string())
}

/// Streamed `/api/generate`: reads Ollama's NDJSON chunks off curl's stdout as they arrive,
/// accumulating `response` fragments. The moment `enough(acc)` says so, the curl process is
/// KILLED — Ollama cancels generation when the client disconnects, so tokens we would throw
/// away are never produced. Returns the accumulated text (partial-by-design), or None on any
/// transport/JSON failure with nothing accumulated.
fn ollama_generate_stream(
    base: &str,
    body: &serde_json::Value,
    enough: impl Fn(&str) -> bool,
) -> Option<String> {
    let mut acc = String::new();
    let ok = ollama_stream(base, "/api/generate", body, |frag| {
        acc.push_str(frag);
        enough(&acc)
    });
    if !ok || acc.is_empty() {
        return None;
    }
    Some(acc)
}

/// The shared NDJSON stream reader under both streamed endpoints. `on_frag` sees each text
/// fragment as it arrives and returns true to stop early (the curl process is killed; Ollama
/// cancels generation on disconnect). Handles both `/api/generate` (`response`) and
/// `/api/chat` (`message.content`) fragment shapes. Returns false on transport/server error.
fn ollama_stream(
    base: &str,
    path: &str,
    body: &serde_json::Value,
    mut on_frag: impl FnMut(&str) -> bool,
) -> bool {
    let Some(payload) = serde_json::to_vec(body).ok() else { return false };
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let Ok(mut child) = std::process::Command::new("curl")
        .args(["-sS", "-N", "-m", "120", "-X", "POST", "--data-binary", "@-", &url])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    use std::io::{BufRead as _, Write as _};
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(&payload).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
    }
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let mut failed = false;
    for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            log::warn!("ollama error: {err}");
            failed = true;
            break;
        }
        let frag = v
            .get("response")
            .and_then(|r| r.as_str())
            .or_else(|| v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()));
        if let Some(frag) = frag {
            if !frag.is_empty() && on_frag(frag) {
                break;
            }
        }
        if v.get("done").and_then(|d| d.as_bool()) == Some(true) {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    !failed
}

/// Tame a raw base-model FIM continuation into ghost text: at most 4 lines (the same contract
/// the Claude prompt enforces), cut at the first line that re-emits the suffix's first non-blank
/// line — base models routinely babble past the cursor, regenerating the code that is already
/// there (`a + b` followed by the closing `}` and a whole fresh `fn main`). None when nothing
/// useful is left.
fn trim_fim(raw: &str, suffix: &str) -> Option<String> {
    let stop = suffix.lines().find(|l| !l.trim().is_empty()).map(str::trim);
    let mut kept = Vec::new();
    for line in raw.lines().take(4) {
        if stop.is_some_and(|s| line.trim() == s) {
            break;
        }
        kept.push(line);
    }
    let text = kept.join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

/// Probe the Ollama server; Some(version) when it answers.
fn ollama_version(base: &str) -> Option<String> {
    let url = format!("{}/api/version", base.trim_end_matches('/'));
    let out = std::process::Command::new("curl")
        .args(["-sS", "-m", "2", &url])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(v.get("version")?.as_str()?.to_string())
}

/// Write `bytes` to `path` owner-only (0600) — the request body carries the user's code and
/// must not be world-readable in a shared /tmp. Returns None (mapping into the caller's `?`)
/// on any failure. On non-unix, falls back to a plain write.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Option<()> {
    use std::io::Write as _;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .ok()?;
        f.write_all(bytes).ok()?;
        return Some(());
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::File::create(path).ok()?;
        f.write_all(bytes).ok()?;
        Some(())
    }
}

/// Strip a markdown fence if the model wrapped its answer anyway.
pub(crate) fn unfence(s: &str) -> String {
    let t = s.trim_matches('\n');
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        return rest.trim_end_matches('`').trim_end_matches('\n').to_string();
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The babble cut: a base model that regenerates the code after the cursor is trimmed to
    /// just the insertion.
    #[test]
    fn trim_fim_cuts_at_suffix_reemission_and_caps_lines() {
        // Real qwen2.5-coder output shape: insertion, then the suffix's `}`, then a fresh main.
        assert_eq!(
            trim_fim("a + b\n}\n\nfn main() {\n    let a = 1;", "\n}").as_deref(),
            Some("a + b")
        );
        // No suffix overlap → capped at 4 lines.
        assert_eq!(trim_fim("l1\nl2\nl3\nl4\nl5\nl6", "").as_deref(), Some("l1\nl2\nl3\nl4"));
        // Nothing left after the cut → None, not an empty ghost.
        assert_eq!(trim_fim("}\nmore", "\n}"), None);
        assert_eq!(trim_fim("   \n", ""), None);
        // Single-line completion with a blank suffix line first: stop line is the } two down.
        assert_eq!(trim_fim("x.len()", "\n\n}").as_deref(), Some("x.len()"));
    }

    #[test]
    fn parse_access_token_shapes() {
        assert_eq!(
            parse_access_token(r#"{"claudeAiOauth":{"accessToken":"sk-abc123"}}"#).as_deref(),
            Some("sk-abc123")
        );
        assert_eq!(parse_access_token(r#"{"claudeAiOauth":{}}"#), None);
        assert_eq!(parse_access_token(r#"{"other":true}"#), None);
        assert_eq!(parse_access_token("not json"), None);
        assert_eq!(
            parse_access_token(r#"{"claudeAiOauth":{"accessToken":42}}"#),
            None
        );
    }
}
