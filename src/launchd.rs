//! Run portmap as a per-user launchd agent: starts at login, restarts if it crashes,
//! runs at reduced CPU priority. No root, no Docker.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const LABEL: &str = "dev.portmap";

fn plist_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn domain() -> String {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    format!("gui/{}", uid.unwrap_or_default())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plist(exe: &Path, port: u16, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>serve</string>
    <string>--port</string>
    <string>{port}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>Nice</key><integer>5</integer>
  <key>LowPriorityIO</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.to_string_lossy()),
        log = xml_escape(&log.to_string_lossy()),
    )
}

/// Homebrew installs into a versioned keg (`<prefix>/Cellar/portmap/0.1.0/bin/portmap`) that
/// `brew upgrade` + `brew cleanup` deletes. Point the agent at the stable `<prefix>/opt/portmap`
/// symlink instead so it survives upgrades.
fn stable_exe(exe: &Path) -> PathBuf {
    let parts: Vec<_> = exe.components().collect();
    if let Some(i) = parts.iter().position(|c| c.as_os_str() == "Cellar") {
        if parts.len() > i + 3 {
            let mut out: PathBuf = parts[..i].iter().collect();
            out.push("opt");
            out.push(parts[i + 1]);
            out.extend(&parts[i + 3..]);
            return out;
        }
    }
    exe.to_path_buf()
}

pub fn install(port: u16, data_dir: &Path) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("`install` uses launchd and is macOS-only; on Linux run `portmap serve` from a systemd user unit".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = stable_exe(&exe.canonicalize().unwrap_or(exe));
    let log = data_dir.join("portmap.log");
    let plist = plist(&exe, port, &log);
    let path = plist_path();
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    // Reinstall cleanly if already loaded (e.g. after upgrading the binary).
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{LABEL}", domain())])
        .output();
    fs::write(&path, plist).map_err(|e| e.to_string())?;
    let out = Command::new("launchctl")
        .args(["bootstrap", &domain(), &path.to_string_lossy()])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    println!("Installed launchd agent {LABEL}");
    println!("  binary: {}", exe.display());
    println!("  plist:  {}", path.display());
    println!("  log:    {}", log.display());
    println!("Open http://localhost:{port}");
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{LABEL}", domain())])
        .output();
    let path = plist_path();
    if path.exists() {
        fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    println!("Removed launchd agent {LABEL}. Data in ~/.portmap was kept.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_metacharacters() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        assert_eq!(xml_escape("plain"), "plain");
    }

    #[test]
    fn plist_runs_serve_on_the_requested_port() {
        let xml = plist(
            Path::new("/Users/x/bin/portmap"),
            9123,
            Path::new("/Users/x/.portmap/portmap.log"),
        );
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<key>Label</key><string>dev.portmap</string>"));
        assert!(xml.contains(
            "<string>/Users/x/bin/portmap</string>\n    <string>serve</string>\n    <string>--port</string>\n    <string>9123</string>"
        ));
        assert!(xml.contains("<key>KeepAlive</key><true/>"));
        assert!(xml
            .contains("<key>StandardOutPath</key><string>/Users/x/.portmap/portmap.log</string>"));
    }

    #[test]
    fn homebrew_keg_maps_to_stable_opt_path() {
        assert_eq!(
            stable_exe(Path::new("/opt/homebrew/Cellar/portmap/0.1.0/bin/portmap")),
            PathBuf::from("/opt/homebrew/opt/portmap/bin/portmap")
        );
        assert_eq!(
            stable_exe(Path::new("/usr/local/Cellar/portmap/0.2.0_1/bin/portmap")),
            PathBuf::from("/usr/local/opt/portmap/bin/portmap")
        );
    }

    #[test]
    fn non_homebrew_paths_are_unchanged() {
        for p in ["/Users/x/.cargo/bin/portmap", "/tmp/Cellar/portmap"] {
            assert_eq!(stable_exe(Path::new(p)), PathBuf::from(p));
        }
    }

    #[test]
    fn plist_escapes_paths() {
        let xml = plist(
            Path::new("/tmp/R&D <dev>/portmap"),
            7878,
            Path::new("/tmp/a&b.log"),
        );
        assert!(xml.contains("<string>/tmp/R&amp;D &lt;dev&gt;/portmap</string>"));
        assert!(xml.contains("<string>/tmp/a&amp;b.log</string>"));
        assert!(!xml.contains("R&D"));
    }
}
