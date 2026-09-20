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

pub fn install(exec_path: &Path, db_path: &str, listen: &str) -> Result<String> {
    let exec_path = exec_path.to_string_lossy();
    if cfg!(target_os = "linux") {
        install_systemd(&exec_path, db_path, listen)
    } else if cfg!(target_os = "macos") {
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

// --- systemd (Linux) --------------------------------------------------

pub fn systemd_unit(exec_path: &str, db_path: &str, listen: &str) -> String {
    format!(
        "[Unit]\n\
         Description=wardn — org & access layer for Mynd\n\
         After=network.target\n\
         \n\
         [Service]\n\
         ExecStart={exec_path} serve\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         Environment=WARDN_DB_PATH={db_path}\n\
         Environment=WARDN_LISTEN_ADDR={listen}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

pub fn resolve_systemd_unit_path(config_dir: Option<&Path>) -> Result<PathBuf> {
    let base = config_dir.context("could not determine a config directory (no $HOME?)")?;
    Ok(base.join("systemd").join("user").join(UNIT_NAME))
}

fn systemd_unit_path() -> Result<PathBuf> {
    resolve_systemd_unit_path(dirs::config_dir().as_deref())
}

fn install_systemd(exec_path: &str, db_path: &str, listen: &str) -> Result<String> {
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

    Ok(format!(
        "Installed {} and started it (enabled to run on login).\n\
         On a headless box, keep it running after logout with:\n\n    \
         loginctl enable-linger $USER\n\n\
         Check on it any time with `wardn service status`.",
        unit_path.display()
    ))
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
        "service: installed ({}) — enabled={} active={}",
        unit_path.display(),
        enabled.as_deref().unwrap_or("unknown"),
        active.as_deref().unwrap_or("unknown"),
    ))
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
        "Installed {} and started it (enabled to run on login).\nLogs: {}\n\
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
        assert!(unit.contains("ExecStart=/home/alice/.cargo/bin/wardn serve"));
        assert!(unit.contains("Environment=WARDN_DB_PATH=/data/org.db"));
        assert!(unit.contains("Environment=WARDN_LISTEN_ADDR=127.0.0.1:7787"));
        assert!(unit.contains("WantedBy=default.target"));
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
}
