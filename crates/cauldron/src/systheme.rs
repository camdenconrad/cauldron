//! OS theme sync — only meaningful on RuneOS. The `System` theme choice follows the desktop's
//! light/dark preference, read through the freedesktop XDG desktop portal
//! (`org.freedesktop.appearance` → `color-scheme`: 0 = no preference, 1 = prefer dark,
//! 2 = prefer light). RuneOS routes Qt/GTK theming through the portal (QT_QPA_PLATFORMTHEME=
//! xdgdesktopportal), so this is the canonical source.
//!
//! Off RuneOS the whole feature is inert: [`is_runeos`] gates it, and `System` resolves to Dark
//! there. Kept intentionally dependency-free (a `gdbus` subprocess, cheap and polled at low
//! frequency) rather than pulling in a D-Bus crate.

use crate::style::Theme;

/// True on RuneOS (`ID=runeos` in `/etc/os-release`). System theme sync is gated on this so
/// other distros don't get a surprise `gdbus` dependency or an unexpected theme flip.
pub fn is_runeos() -> bool {
    std::fs::read_to_string("/etc/os-release")
        .map(|s| s.lines().any(|l| l.trim() == "ID=runeos"))
        .unwrap_or(false)
}

/// The OS's current light/dark preference as a concrete theme, or `None` when it can't be
/// read (portal missing, no preference set). Dark/Light only — the OS exposes a binary
/// preference, not our named palettes.
pub fn os_theme() -> Option<Theme> {
    if !is_runeos() {
        return None;
    }
    let out = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.portal.Desktop",
            "--object-path",
            "/org/freedesktop/portal/desktop",
            "--method",
            "org.freedesktop.portal.Settings.Read",
            "org.freedesktop.appearance",
            "color-scheme",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_color_scheme(&String::from_utf8_lossy(&out.stdout))
}

/// Resolve a persisted [`crate::settings::ThemeChoice`] to a concrete [`Theme`]. `System`
/// consults the OS (RuneOS) and falls back to Dark; every other choice maps directly.
pub fn resolve(choice: crate::settings::ThemeChoice) -> Theme {
    use crate::settings::ThemeChoice as Tc;
    match choice {
        Tc::Dark => Theme::Dark,
        Tc::Light => Theme::Light,
        Tc::Midnight => Theme::Midnight,
        Tc::Amber => Theme::Amber,
        Tc::System => os_theme().unwrap_or(Theme::Dark),
    }
}

/// Parse the portal reply, e.g. `(<<uint32 1>>,)` → 1 = dark, 2 = light, 0/other = None.
fn parse_color_scheme(reply: &str) -> Option<Theme> {
    let digits: String = reply.chars().filter(|c| c.is_ascii_digit()).collect();
    // The reply may contain the "32" from "uint32"; take the LAST run of digits after it.
    let n = reply.rsplit("uint32").next()?.chars().filter(|c| c.is_ascii_digit()).collect::<String>();
    let val: u32 = n.trim().parse().ok().or_else(|| digits.trim().parse().ok())?;
    match val {
        1 => Some(Theme::Dark),
        2 => Some(Theme::Light),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_portal_replies() {
        assert_eq!(parse_color_scheme("(<<uint32 1>>,)"), Some(Theme::Dark));
        assert_eq!(parse_color_scheme("(<<uint32 2>>,)"), Some(Theme::Light));
        assert_eq!(parse_color_scheme("(<<uint32 0>>,)"), None);
        assert_eq!(parse_color_scheme("garbage"), None);
    }
}
