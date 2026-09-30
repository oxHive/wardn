//! `wardn service {install,uninstall,status}` — runs `wardn serve` under
//! the platform's native service manager: a systemd `--user` unit on
//! Linux, a launchd LaunchAgent on macOS.
//!
//! Deliberately user-level, never root/`sudo`/system-wide: wardn's default
//! database lives under the invoking user's home directory (see
//! `config::default_db_path`), so the service that reads and writes it
//! runs as that same user. This is also the reason `install` always bakes
//! `WARDN_DB_PATH`/`WARDN_LISTEN_ADDR` into the service definition rather
//! than leaving them to be inherited from the environment — neither
//! systemd user units nor launchd agents inherit the shell's environment,
//! so a value only set in `~/.bashrc` would otherwise silently vanish for
//! the backgrounded process.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const UNIT_NAME: &str = "wardn.service";
const LABEL: &str = "com.oxhive.wardn";

pub fn install(
    exec_path: &Path,
    db_path: &str,
    listen: &str,
    enable_linger: bool,
) -> Result<String> {
    let exec_path = exec_path.to_string_lossy();
    if cfg!(target_os = "linux") {
        install_systemd(&exec_path, db_path, listen, enable_linger)
    } else if cfg!(target_os = "macos") {
        // launchd has no linger equivalent — a LaunchAgent only ever starts
        // on login, so the flag is meaningless here (see `install_launchd`'s
        // doc comment).
        install_launchd(&exec_path, db_path, listen)
    } else {
        bail!("`wardn service` supports Linux (systemd) and macOS (launchd) only");
    }
}

pub fn uninstall() -> Result<String> {
    if cfg!(target_os = "linux") {
        uninstall_systemd()
    } else if cfg!(target_os = "macos") {
        uninstall_launchd()
    } else {
        bail!("`wardn service` supports Linux (systemd) and macOS (launchd) only");
    }
}

pub fn os_status() -> Result<String> {
    if cfg!(target_os = "linux") {
        systemd_status()
    } else if cfg!(target_os = "macos") {
        launchd_status()
    } else {
        bail!("`wardn service` supports Linux (systemd) and macOS (launchd) only");
    }
}

/// The listen address the installed service was configured with, if a
/// service definition exists and it can be read — `wardn service install
/// --listen` bakes this in, so it can differ from the shell's default.
pub fn installed_listen_addr() -> Option<String> {
    if cfg!(target_os = "linux") {
        let unit = std::fs::read_to_string(systemd_unit_path().ok()?).ok()?;
        listen_addr_from_systemd_unit(&unit)
    } else if cfg!(target_os = "macos") {
        let plist = std::fs::read_to_string(launchd_plist_path().ok()?).ok()?;
        listen_addr_from_launchd_plist(&plist)
    } else {
        None
    }
}

// --- systemd (Linux) --------------------------------------------------

pub fn systemd_unit(exec_path: &str, db_path: &str, listen: &str) -> String {
    // Every value is double-quoted and escaped: systemd splits unquoted
    // values on whitespace, expands `%` specifiers everywhere, and expands
    // `$VAR` in `ExecStart=`, so a path like `/home/me/My Data/org.db`
    // would otherwise be cut short or rewritten.
    let exec_path = systemd_quote(&exec_path.replace('$', "$$"));
    let db_env = systemd_quote(&format!("WARDN_DB_PATH={db_path}"));
    let listen_env = systemd_quote(&format!("{LISTEN_ENV_PREFIX}{listen}"));
    format!(
        "[Unit]\n\
         Description=wardn — org & access layer for Mynd\n\
         After=network.target\n\
         \n\
         [Service]\n\
         ExecStart={exec_path} serve\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         Environment={db_env}\n\
         Environment={listen_env}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

const LISTEN_ENV_PREFIX: &str = "WARDN_LISTEN_ADDR=";

/// Wraps `s` in double quotes, C-escaping `\`, `"` and newlines and
/// doubling `%` so systemd reads it back as exactly one literal word.
fn systemd_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Inverse of `systemd_quote` for one `Environment=` value; also accepts
/// the unquoted form older `wardn service install`s wrote.
fn systemd_unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some(next)) => {
                chars.next();
                out.push(if next == 'n' { '\n' } else { next });
            }
            ('%', Some('%')) => {
                chars.next();
                out.push('%');
            }
            (c, _) => out.push(c),
        }
    }
    out
}

/// The `WARDN_LISTEN_ADDR` baked into an installed systemd unit.
pub fn listen_addr_from_systemd_unit(unit: &str) -> Option<String> {
    unit.lines()
        .filter_map(|line| line.trim().strip_prefix("Environment="))
        .map(systemd_unquote)
        .find_map(|value| value.strip_prefix(LISTEN_ENV_PREFIX).map(str::to_string))
}

pub fn resolve_systemd_unit_path(config_dir: Option<&Path>) -> Result<PathBuf> {
    let base = config_dir.context("could not determine a config directory (no $HOME?)")?;
    Ok(base.join("systemd").join("user").join(UNIT_NAME))
}

fn systemd_unit_path() -> Result<PathBuf> {
    resolve_systemd_unit_path(dirs::config_dir().as_deref())
}

fn install_systemd(
    exec_path: &str,
    db_path: &str,
    listen: &str,
    enable_linger: bool,
) -> Result<String> {
    let unit_path = systemd_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&unit_path, systemd_unit(exec_path, db_path, listen))
        .with_context(|| format!("writing {}", unit_path.display()))?;

    run_ok(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    run_ok(Command::new("systemctl").args(["--user", "enable", "--now", UNIT_NAME]))?;
    // `enable --now` only starts the unit if it wasn't already running, so
    // a reinstall (e.g. after `--listen` or `--db` changed) needs an
    // explicit restart to pick up the new Environment= lines.
    run_ok(Command::new("systemctl").args(["--user", "restart", UNIT_NAME]))?;

    // `WantedBy=default.target` alone only starts the unit when this user
    // logs in — a systemd --user manager doesn't run at boot unless
    // lingering is on. Without it, a headless box that reboots stays down
    // until someone logs in again.
    let linger = if enable_linger {
        enable_linger_now()
    } else {
        LingerOutcome::Skipped
    };

    Ok(format!(
        "Installed {} and started it (enabled to run on login).\n{}\n\
         Check on it any time with `wardn service status`.",
        unit_path.display(),
        linger_note(&linger),
    ))
}

enum LingerOutcome {
    Skipped,
    Enabled,
    Failed(String),
}

fn enable_linger_now() -> LingerOutcome {
    match Command::new("loginctl").arg("enable-linger").output() {
        Ok(out) if out.status.success() => LingerOutcome::Enabled,
        Ok(out) => LingerOutcome::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Err(e) => LingerOutcome::Failed(e.to_string()),
    }
}

fn linger_note(outcome: &LingerOutcome) -> String {
    match outcome {
        LingerOutcome::Enabled => {
            "Linger enabled — this also starts the service at boot, without needing a login."
                .to_string()
        }
        LingerOutcome::Skipped => {
            "Linger not enabled (--no-linger) — the service starts on login, not at boot. \
             Enable it later with `loginctl enable-linger $USER`."
                .to_string()
        }
        LingerOutcome::Failed(err) => format!(
            "Could not enable linger automatically ({err}) — the service still starts on \
             login, but not at boot until you run `loginctl enable-linger $USER` yourself."
        ),
    }
}

fn uninstall_systemd() -> Result<String> {
    let unit_path = systemd_unit_path()?;
    if !unit_path.exists() {
        return Ok(format!(
            "{} is not installed — nothing to do",
            unit_path.display()
        ));
    }
    // Best-effort: an already-stopped or half-broken unit shouldn't block
    // removing its file.
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", UNIT_NAME])
        .output();
    std::fs::remove_file(&unit_path)
        .with_context(|| format!("removing {}", unit_path.display()))?;
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    Ok(format!("Stopped and removed {}", unit_path.display()))
}

fn systemd_status() -> Result<String> {
    let unit_path = systemd_unit_path()?;
    if !unit_path.exists() {
        return Ok(format!(
            "service: not installed (expected {})",
            unit_path.display()
        ));
    }
    let enabled =
        command_stdout(Command::new("systemctl").args(["--user", "is-enabled", UNIT_NAME]));
    let active = command_stdout(Command::new("systemctl").args(["--user", "is-active", UNIT_NAME]));
    Ok(format!(
        "service: installed ({}) — enabled={} active={}\n{}",
        unit_path.display(),
        enabled.as_deref().unwrap_or("unknown"),
        active.as_deref().unwrap_or("unknown"),
        linger_status_line(),
    ))
}

fn linger_status_line() -> String {
    let username = command_stdout(&mut Command::new("whoami"));
    let linger = username.as_deref().and_then(|user| {
        command_stdout(Command::new("loginctl").args([
            "show-user",
            "--value",
            "-p",
            "Linger",
            user,
        ]))
    });
    match linger.as_deref() {
        Some("yes") => {
            "linger: enabled — this service also starts at boot, without needing a login"
                .to_string()
        }
        Some("no") => "linger: disabled — starts on login only (`loginctl enable-linger $USER` \
                        to also start at boot)"
            .to_string(),
        _ => "linger: unknown".to_string(),
    }
}

// --- launchd (macOS) ----------------------------------------------------

pub fn launchd_plist(exec_path: &str, db_path: &str, listen: &str, log_path: &str) -> String {
    let exec_path = xml_escape(exec_path);
    let db_path = xml_escape(db_path);
    let listen = xml_escape(listen);
    let log_path = xml_escape(log_path);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exec_path}</string>
        <string>serve</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>WARDN_DB_PATH</key>
        <string>{db_path}</string>
        <key>WARDN_LISTEN_ADDR</key>
        <string>{listen}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_path}</string>
    <key>StandardErrorPath</key>
    <string>{log_path}</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The `WARDN_LISTEN_ADDR` baked into an installed launchd plist.
pub fn listen_addr_from_launchd_plist(plist: &str) -> Option<String> {
    let after_key = plist.split_once("<key>WARDN_LISTEN_ADDR</key>")?.1;
    let value = after_key.trim_start().strip_prefix("<string>")?;
    let (value, _) = value.split_once("</string>")?;
    Some(xml_unescape(value))
}

pub fn resolve_launchd_plist_path(home_dir: Option<&Path>) -> Result<PathBuf> {
    let base = home_dir.context("could not determine the home directory")?;
    Ok(base
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn launchd_plist_path() -> Result<PathBuf> {
    resolve_launchd_plist_path(dirs::home_dir().as_deref())
}

pub fn resolve_launchd_log_path(data_dir: Option<&Path>) -> Result<PathBuf> {
    let base = data_dir.context("could not determine a local data directory")?;
    Ok(base.join("wardn").join("service.log"))
}

fn launchd_log_path() -> Result<PathBuf> {
    resolve_launchd_log_path(dirs::data_local_dir().as_deref())
}

/// Unlike a systemd --user unit, a LaunchAgent has no lingering
/// equivalent: it only ever starts when this user's launchd session
/// starts, which happens on login (interactive or auto-login), never
/// unattended at boot. Surviving a reboot without a login requires either
/// enabling auto-login for this user, or a root-owned LaunchDaemon
/// instead — both outside what `wardn service install` does, since it's
/// deliberately user-level.
fn install_launchd(exec_path: &str, db_path: &str, listen: &str) -> Result<String> {
    let plist_path = launchd_plist_path()?;
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let log_path = launchd_log_path()?;
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let contents = launchd_plist(exec_path, db_path, listen, &log_path.to_string_lossy());
    std::fs::write(&plist_path, contents)
        .with_context(|| format!("writing {}", plist_path.display()))?;

    // Best-effort: unload any stale copy (e.g. from a previous install)
    // before loading the fresh one, so a reinstall actually picks up
    // changed Environment values instead of launchd keeping the old ones.
    let _ = Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .output();
    run_ok(Command::new("launchctl").args(["load", "-w", &plist_path.to_string_lossy()]))?;

    Ok(format!(
        "Installed {} and started it (enabled to run on login).\n\
         This starts again on login, not unattended at boot — see \
         auto-login if this needs to survive a reboot with nobody signed in.\n\
         Logs: {}\n\
         Check on it any time with `wardn service status`.",
        plist_path.display(),
        log_path.display(),
    ))
}

fn uninstall_launchd() -> Result<String> {
    let plist_path = launchd_plist_path()?;
    if !plist_path.exists() {
        return Ok(format!(
            "{} is not installed — nothing to do",
            plist_path.display()
        ));
    }
    let _ = Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .output();
    std::fs::remove_file(&plist_path)
        .with_context(|| format!("removing {}", plist_path.display()))?;
    Ok(format!("Stopped and removed {}", plist_path.display()))
}

fn launchd_status() -> Result<String> {
    let plist_path = launchd_plist_path()?;
    if !plist_path.exists() {
        return Ok(format!(
            "service: not installed (expected {})",
            plist_path.display()
        ));
    }
    match Command::new("launchctl").args(["list", LABEL]).output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            Ok(format!(
                "service: installed ({}) — loaded\n{}",
                plist_path.display(),
                text.trim()
            ))
        }
        _ => Ok(format!(
            "service: installed ({}) but not loaded — run `wardn service install` again",
            plist_path.display()
        )),
    }
}

// --- shared helpers -------------------------------------------------------

fn run_ok(cmd: &mut Command) -> Result<()> {
    let program = format!("{cmd:?}");
    let output = cmd.output().with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn command_stdout(cmd: &mut Command) -> Option<String> {
    let text = cmd
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_bakes_in_exec_path_and_env() {
        let unit = systemd_unit(
            "/home/alice/.cargo/bin/wardn",
            "/data/org.db",
            "127.0.0.1:7787",
        );
        assert!(unit.contains("ExecStart=\"/home/alice/.cargo/bin/wardn\" serve"));
        assert!(unit.contains("Environment=\"WARDN_DB_PATH=/data/org.db\""));
        assert!(unit.contains("Environment=\"WARDN_LISTEN_ADDR=127.0.0.1:7787\""));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn systemd_unit_quotes_spaces_and_escapes_specifiers() {
        let unit = systemd_unit(
            "/home/al/My Apps/$bin/wardn",
            "/home/al/My Data/100%\\\"x\".db",
            "127.0.0.1:7787",
        );
        assert!(unit.contains("ExecStart=\"/home/al/My Apps/$$bin/wardn\" serve"));
        assert!(
            unit.contains("Environment=\"WARDN_DB_PATH=/home/al/My Data/100%%\\\\\\\"x\\\".db\"")
        );
    }

    #[test]
    fn listen_addr_round_trips_through_the_systemd_unit() {
        let unit = systemd_unit("/bin/wardn", "/data/org.db", "127.0.0.1:9000");
        assert_eq!(
            listen_addr_from_systemd_unit(&unit).as_deref(),
            Some("127.0.0.1:9000")
        );
    }

    #[test]
    fn listen_addr_is_read_from_older_unquoted_units() {
        let unit = "[Service]\nEnvironment=WARDN_LISTEN_ADDR=0.0.0.0:8000\n";
        assert_eq!(
            listen_addr_from_systemd_unit(unit).as_deref(),
            Some("0.0.0.0:8000")
        );
    }

    #[test]
    fn listen_addr_round_trips_through_the_launchd_plist() {
        let plist = launchd_plist("/bin/wardn", "/data/org.db", "[::1]:9000", "/log");
        assert_eq!(
            listen_addr_from_launchd_plist(&plist).as_deref(),
            Some("[::1]:9000")
        );
    }

    #[test]
    fn launchd_plist_bakes_in_exec_path_and_env() {
        let plist = launchd_plist(
            "/opt/homebrew/bin/wardn",
            "/data/org.db",
            "127.0.0.1:7787",
            "/data/service.log",
        );
        assert!(plist.contains("<string>com.oxhive.wardn</string>"));
        assert!(plist.contains("<string>/opt/homebrew/bin/wardn</string>"));
        assert!(plist.contains("<key>WARDN_DB_PATH</key>\n        <string>/data/org.db</string>"));
        assert!(
            plist.contains("<key>WARDN_LISTEN_ADDR</key>\n        <string>127.0.0.1:7787</string>")
        );
        assert!(plist.contains("<string>/data/service.log</string>"));
    }

    #[test]
    fn launchd_plist_escapes_xml_special_characters() {
        let plist = launchd_plist("/bin/wardn", "/data/a&b.db", "127.0.0.1:7787", "/log");
        assert!(plist.contains("/data/a&amp;b.db"));
        assert!(!plist.contains("/data/a&b.db"));
    }

    #[test]
    fn resolve_systemd_unit_path_lives_under_systemd_user() {
        let path = resolve_systemd_unit_path(Some(Path::new("/home/alice/.config"))).unwrap();
        assert_eq!(
            path,
            Path::new("/home/alice/.config/systemd/user/wardn.service")
        );
    }

    #[test]
    fn resolve_systemd_unit_path_errors_without_a_config_dir() {
        assert!(resolve_systemd_unit_path(None).is_err());
    }

    #[test]
    fn resolve_launchd_plist_path_lives_under_launch_agents() {
        let path = resolve_launchd_plist_path(Some(Path::new("/Users/alice"))).unwrap();
        assert_eq!(
            path,
            Path::new("/Users/alice/Library/LaunchAgents/com.oxhive.wardn.plist")
        );
    }

    #[test]
    fn resolve_launchd_plist_path_errors_without_a_home_dir() {
        assert!(resolve_launchd_plist_path(None).is_err());
    }

    #[test]
    fn resolve_launchd_log_path_lives_under_the_wardn_data_dir() {
        let path =
            resolve_launchd_log_path(Some(Path::new("/Users/alice/Library/Application Support")))
                .unwrap();
        assert_eq!(
            path,
            Path::new("/Users/alice/Library/Application Support/wardn/service.log")
        );
    }

    #[test]
    fn resolve_launchd_log_path_errors_without_a_data_dir() {
        assert!(resolve_launchd_log_path(None).is_err());
    }

    #[test]
    fn command_stdout_trims_and_returns_none_on_spawn_failure() {
        assert_eq!(
            command_stdout(&mut Command::new("definitely-not-a-real-binary-xyz")),
            None
        );
    }

    #[test]
    fn command_stdout_treats_empty_output_as_none() {
        // e.g. `systemctl --user is-active` when there's no session bus to
        // connect to at all — it exits non-zero with nothing on stdout,
        // which should read as "unknown", not a blank status line.
        assert_eq!(command_stdout(&mut Command::new("true")), None);
    }

    #[test]
    fn linger_note_enabled_mentions_boot() {
        assert!(linger_note(&LingerOutcome::Enabled).contains("boot"));
    }

    #[test]
    fn linger_note_skipped_explains_how_to_enable_it_later() {
        let note = linger_note(&LingerOutcome::Skipped);
        assert!(note.contains("--no-linger"));
        assert!(note.contains("loginctl enable-linger $USER"));
    }

    #[test]
    fn linger_note_failed_surfaces_the_underlying_error() {
        let note = linger_note(&LingerOutcome::Failed(
            "Interactive authentication required.".into(),
        ));
        assert!(note.contains("Interactive authentication required."));
        assert!(note.contains("loginctl enable-linger $USER"));
    }
}
