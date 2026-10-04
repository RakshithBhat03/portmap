//! Thumbnail capture via a headless Chromium-family browser that is already installed.
//! Captures run one at a time on a single worker thread so at most one browser process
//! exists at any moment, and each is killed if it exceeds the timeout.

use crate::store::{now, Store};
use crate::Shared;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(25);

pub fn find_browser() -> Option<PathBuf> {
    if std::env::var("PORTMAP_NO_THUMBS").is_ok_and(|v| v == "1") {
        return None;
    }
    if let Ok(p) = std::env::var("PORTMAP_BROWSER") {
        return Some(PathBuf::from(p)).filter(|p| p.exists());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let apps = [
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Brave Browser.app/Contents/MacOS/Brave Browser",
        "Chromium.app/Contents/MacOS/Chromium",
        "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "Vivaldi.app/Contents/MacOS/Vivaldi",
    ];
    let bundles = apps.iter().flat_map(|a| {
        [
            PathBuf::from("/Applications").join(a),
            PathBuf::from(&home).join("Applications").join(a),
        ]
    });
    let linux = [
        "google-chrome",
        "chromium",
        "chromium-browser",
        "brave-browser",
        "microsoft-edge",
    ]
    .iter()
    .flat_map(|b| ["/usr/bin", "/usr/local/bin", "/snap/bin"].map(|d| Path::new(d).join(b)));
    bundles.chain(linux).find(|p| p.exists())
}

pub fn run(shared: Arc<Shared>, rx: Receiver<u16>) {
    let Some(browser) = shared.cfg.browser.clone() else {
        return;
    };
    let dir = shared.cfg.data_dir.clone();
    let _ = fs::create_dir_all(dir.join("thumbs"));
    while let Ok(port) = rx.recv() {
        let url = {
            let mut store = shared.store.lock().unwrap();
            if !store.thumbs_wanted {
                // Thumbnails were switched off after this was queued.
                store.thumb_queue.remove(&port);
                continue;
            }
            store
                .entries
                .get(&port)
                .filter(|e| e.shows_card())
                .map(|e| e.url())
        };
        let ok = match url {
            Some(url) => capture(&browser, &url, &Store::thumb_path(&dir, port)),
            None => false,
        };
        let mut store = shared.store.lock().unwrap();
        store.thumb_queue.remove(&port);
        if let Some(e) = store.entries.get_mut(&port) {
            if ok {
                e.thumb_at = now();
                e.thumb_failed_at = 0;
            } else if e.listening {
                e.thumb_failed_at = now();
            }
        } else {
            // Forgotten while we were capturing.
            let _ = fs::remove_file(Store::thumb_path(&dir, port));
        }
        store.refresh_view();
        store.save();
    }
}

fn capture(browser: &Path, url: &str, out: &Path) -> bool {
    let work = std::env::temp_dir().join(format!("portmap-shot-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    if fs::create_dir_all(&work).is_err() {
        return false;
    }
    let png = work.join("shot.png");
    let child = Command::new(browser)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--mute-audio",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-sync",
            "--disable-component-update",
            "--window-size=1280,800",
            "--virtual-time-budget=4000",
        ])
        .arg(format!(
            "--user-data-dir={}",
            work.join("profile").display()
        ))
        .arg(format!("--screenshot={}", png.display()))
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    // Recent Chrome versions write the screenshot and then linger, so success is "the
    // PNG exists and stopped growing", not "the process exited".
    let ok = match child {
        Ok(mut c) => {
            let start = Instant::now();
            let mut last_len = 0;
            let ok = loop {
                let len = fs::metadata(&png).map(|m| m.len()).unwrap_or(0);
                if len > 0 && len == last_len {
                    break true;
                }
                last_len = len;
                match c.try_wait() {
                    Ok(Some(_)) => break fs::metadata(&png).is_ok_and(|m| m.len() > 0),
                    Ok(None) if start.elapsed() < CAPTURE_TIMEOUT => {
                        thread::sleep(Duration::from_millis(250))
                    }
                    _ => break false,
                }
            };
            // Take down the browser and all of its helper processes.
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(format!("-{}", c.id()))
                .status();
            let grace = Instant::now();
            while c.try_wait().ok().flatten().is_none() && grace.elapsed() < Duration::from_secs(2)
            {
                thread::sleep(Duration::from_millis(100));
            }
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{}", c.id()))
                .stderr(Stdio::null())
                .status();
            let _ = c.wait();
            ok
        }
        Err(_) => false,
    };
    let ok = ok && shrink(&png, out);
    let _ = fs::remove_dir_all(&work);
    ok
}

/// Downscale to a ~40 KB JPEG. `sips` ships with macOS; elsewhere keep the PNG as-is
/// (browsers sniff the content type, so the .jpg name is harmless).
fn shrink(png: &Path, out: &Path) -> bool {
    let tmp = out.with_extension("tmp");
    let resized = Command::new("sips")
        .args([
            "-s",
            "format",
            "jpeg",
            "-s",
            "formatOptions",
            "70",
            "--resampleWidth",
            "640",
        ])
        .arg(png)
        .arg("--out")
        .arg(&tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !resized && fs::copy(png, &tmp).is_err() {
        return false;
    }
    fs::rename(&tmp, out).is_ok()
}
