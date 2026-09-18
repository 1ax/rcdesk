//! Cross-platform, cfg-free logic behind "start at login" (slice 2.6d):
//! resolving which path autostart should point at, plus the pure
//! string-manipulation each platform's mechanism is built from -- macOS
//! LaunchAgent plist XML (generation, escaping, and parsing our own
//! generated format back out) and Windows `Run` registry value formatting
//! and comparison. `platform::macos::autostart`/`platform::windows::autostart`
//! only wrap this with actual file/registry IO, so this half is unit
//! testable on any OS (see the plist/Run tests below); the platform halves
//! additionally have their own tests using a temp directory / can't be
//! tested at all without a Windows machine, respectively.
//!
//! Off by default everywhere: nothing in this module or its platform
//! counterparts runs at startup -- only `rcdesk-agent`'s "Start at login"
//! menu item and `rcdesk-host autostart on` call `enable`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

/// Resolves the absolute path autostart (the LaunchAgent plist / the `Run`
/// registry value) should point at.
///
/// Two callers, two different starting points, same function: `rcdesk-agent`
/// calls this from its own "Start at login" menu item, where the answer is
/// just its own `current_exe()`. `rcdesk-host`'s `autostart` CLI subcommand
/// calls it too, where `current_exe()` is `rcdesk-host` itself and the right
/// answer is instead the sibling `rcdesk-agent[.exe]` shipped in the same
/// directory (see docs/dev-run.md, docs/host-windows.md) -- an error with a
/// clear message if it isn't there.
///
/// Either way the result is canonicalized (`std::fs::canonicalize`) so a
/// relative or symlinked `current_exe()` still yields a path launchd/the
/// registry can launch directly; on Windows `canonicalize` prepends the
/// "verbatim" `\\?\` prefix, which `strip_verbatim_prefix` removes again --
/// see its own doc comment for why that prefix can't be left in.
pub fn agent_path() -> anyhow::Result<PathBuf> {
    let current =
        std::env::current_exe().context("failed to determine the current executable's path")?;

    let is_agent_binary = current
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem == "rcdesk-agent")
        .unwrap_or(false);

    let target = if is_agent_binary {
        current
    } else {
        let dir = current
            .parent()
            .context("the current executable has no parent directory")?;
        let candidate = sibling_agent_path(dir, cfg!(target_os = "windows"));
        if !candidate.exists() {
            bail!(
                "rcdesk-agent not found at {} -- it ships next to rcdesk-host, see docs/host-windows.md",
                candidate.display()
            );
        }
        candidate
    };

    let canonical = std::fs::canonicalize(&target)
        .with_context(|| format!("failed to canonicalize {}", target.display()))?;
    Ok(strip_verbatim_prefix(&canonical))
}

/// `rcdesk-agent`'s binary filename for the current OS. A plain parameter
/// (rather than reading `cfg!(target_os = "windows")` itself) so both
/// branches are testable from a single, OS-independent test run.
pub fn agent_binary_name(windows: bool) -> &'static str {
    if windows {
        "rcdesk-agent.exe"
    } else {
        "rcdesk-agent"
    }
}

/// Where `rcdesk-agent`'s binary should be, given the directory the other
/// binary of this same build (`rcdesk-host`, normally) was launched from.
/// Pure -- no filesystem access -- so `agent_path` above is the only part of
/// path resolution that needs a real executable on disk to test.
pub fn sibling_agent_path(exe_dir: &Path, windows: bool) -> PathBuf {
    exe_dir.join(agent_binary_name(windows))
}

/// Strips `std::fs::canonicalize`'s Windows "verbatim" prefix --
/// `\\?\C:\foo` -> `C:\foo`, `\\?\UNC\server\share` -> `\\server\share` --
/// which launchd doesn't apply to (this runs on macOS too, where it's
/// always a no-op) and the `Run` registry value must not carry: neither
/// launchd nor a plain `CreateProcess`-style launch-at-login entry
/// understands the verbatim form. Pure string manipulation, hence testable
/// on any OS without a real Windows path to canonicalize.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

// --- macOS: LaunchAgent plist XML -------------------------------------------

const LAUNCH_AGENT_LABEL: &str = "app.rcdesk.agent";

/// Escapes the five XML predefined entities -- the only untrusted text
/// `generate_plist` embeds is a filesystem path, so this (not a full XML
/// writer) is all `enable` needs.
pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Builds `app.rcdesk.agent.plist`'s full content. `RunAtLoad=true` starts
/// it at every login; `KeepAlive.SuccessfulExit=false` restarts it only
/// after it dies unexpectedly (exit code != 0), not after a clean "Quit"
/// (exit 0) from its own menu -- see slice 2.6d's plan. `ProcessType
/// Interactive` + `LimitLoadToSessionType Aqua` keep it tied to the GUI
/// login session it's meant for, same as manually checking "Open at Login"
/// in System Settings would.
pub fn generate_plist(agent: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{path}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#,
        label = LAUNCH_AGENT_LABEL,
        path = escape_xml(&agent.to_string_lossy()),
    )
}

/// Parses `ProgramArguments[0]` back out of a plist generated by
/// `generate_plist` above. Not a general plist parser -- just enough to
/// round-trip our own fixed format, which is all
/// `platform::macos::autostart::is_enabled` needs to compare the stored path
/// against the current one.
pub fn parse_program_arguments_path(xml: &str) -> Option<String> {
    let after_key = xml.split_once("<key>ProgramArguments</key>")?.1;
    let after_array = after_key.split_once("<array>")?.1;
    let after_string = after_array.split_once("<string>")?.1;
    let (inner, _) = after_string.split_once("</string>")?;
    Some(unescape_xml(inner))
}

// --- Windows: HKCU Run value -------------------------------------------

/// The value name `platform::windows::autostart` reads/writes under
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub const RUN_VALUE_NAME: &str = "rcdesk";

/// The literal string `enable` writes to (and `is_enabled` compares
/// against) the `rcdesk` `Run` value: the agent's path in double quotes, the
/// form Explorer/`CreateProcess` expect for a path that might contain
/// spaces.
pub fn format_run_value(agent: &Path) -> String {
    format!("\"{}\"", agent.display())
}

/// Whether a `Run` value already read back from the registry (`stored`)
/// points at `agent`, comparing case-insensitively -- Windows paths are, and
/// a reinstall to a differently-cased path component shouldn't make an
/// already-enabled autostart read back as off.
pub fn run_value_matches(stored: &str, agent: &Path) -> bool {
    stored.to_lowercase() == format_run_value(agent).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_agent_path_picks_name_by_os() {
        let dir = Path::new("/opt/rcdesk");
        assert_eq!(
            sibling_agent_path(dir, false),
            PathBuf::from("/opt/rcdesk/rcdesk-agent")
        );
        assert_eq!(
            sibling_agent_path(dir, true),
            PathBuf::from("/opt/rcdesk/rcdesk-agent.exe")
        );
    }

    #[test]
    fn strip_verbatim_prefix_removes_local_prefix() {
        let path = Path::new(r"\\?\C:\Program Files\rcdesk\rcdesk-agent.exe");
        assert_eq!(
            strip_verbatim_prefix(path),
            PathBuf::from(r"C:\Program Files\rcdesk\rcdesk-agent.exe")
        );
    }

    #[test]
    fn strip_verbatim_prefix_rewrites_unc_prefix() {
        let path = Path::new(r"\\?\UNC\server\share\rcdesk-agent.exe");
        assert_eq!(
            strip_verbatim_prefix(path),
            PathBuf::from(r"\\server\share\rcdesk-agent.exe")
        );
    }

    #[test]
    fn strip_verbatim_prefix_is_a_no_op_without_the_prefix() {
        let path = Path::new("/Applications/rcdesk-agent");
        assert_eq!(strip_verbatim_prefix(path), path.to_path_buf());
    }

    #[test]
    fn generate_plist_contains_all_keys() {
        let xml = generate_plist(Path::new("/Applications/rcdesk-agent"));
        for needle in [
            "<key>Label</key>",
            "<string>app.rcdesk.agent</string>",
            "<key>ProgramArguments</key>",
            "<array>",
            "<string>/Applications/rcdesk-agent</string>",
            "<key>RunAtLoad</key>",
            "<true/>",
            "<key>KeepAlive</key>",
            "<key>SuccessfulExit</key>",
            "<false/>",
            "<key>ProcessType</key>",
            "<string>Interactive</string>",
            "<key>LimitLoadToSessionType</key>",
            "<string>Aqua</string>",
        ] {
            assert!(xml.contains(needle), "missing {needle:?} in:\n{xml}");
        }
    }

    #[test]
    fn escape_xml_escapes_all_five_entities() {
        assert_eq!(
            escape_xml(r#"a & b < c > d " e ' f"#),
            "a &amp; b &lt; c &gt; d &quot; e &apos; f"
        );
    }

    #[test]
    fn plist_round_trips_a_path_with_special_characters() {
        let path = Path::new("/Users/a & b/<rcdesk> \"agent\"/rcdesk-agent");
        let xml = generate_plist(path);
        assert_eq!(
            parse_program_arguments_path(&xml).as_deref(),
            Some(path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn plist_round_trips_a_plain_path() {
        let path = Path::new("/Applications/rcdesk-agent");
        let xml = generate_plist(path);
        assert_eq!(
            parse_program_arguments_path(&xml).as_deref(),
            Some(path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn format_run_value_quotes_the_path() {
        assert_eq!(
            format_run_value(Path::new(r"C:\rcdesk\rcdesk-agent.exe")),
            r#""C:\rcdesk\rcdesk-agent.exe""#
        );
    }

    #[test]
    fn run_value_matches_is_case_insensitive() {
        let agent = Path::new(r"C:\rcdesk\rcdesk-agent.exe");
        assert!(run_value_matches(r#""c:\rcdesk\rcdesk-agent.exe""#, agent));
        assert!(!run_value_matches(r#""C:\other\rcdesk-agent.exe""#, agent));
    }
}
