//! Run portmap as a per-user launchd agent: starts at login, restarts if it crashes,
//! runs at reduced CPU priority. No root, no Docker.

use std::fs;
use std::path::PathBuf;
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

pub fn install(port: u16, data_dir: &std::path::Path) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("`install` uses launchd and is macOS-only; on Linux run `portmap serve` from a systemd user unit".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let log = data_dir.join("portmap.log");
    let plist = format!(
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
    );
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
