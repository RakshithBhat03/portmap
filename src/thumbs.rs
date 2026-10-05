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
                .filter(|e| e.shows_card() && !e.hidden)
                .map(|e| e.url())
        };
        let ok = match url {
            Some(url) => capture(&browser, &url, &Store::thumb_path(&dir, port)),
            None => false,
        };
        let mut store = shared.store.lock().unwrap();
        store.thumb_queue.remove(&port);
        match store.entries.get_mut(&port) {
            Some(e) if !e.hidden => {
                if ok {
                    e.thumb_at = now();
                    e.thumb_failed_at = 0;
                } else if e.listening {
                    e.thumb_failed_at = now();
                }
            }
            // Forgotten or hidden while we were capturing: leave no picture behind.
            _ => {
                let _ = fs::remove_file(Store::thumb_path(&dir, port));
            }
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
                .stderr(Stdio::null())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Entry;
    use crate::testutil::{harness, TempDir};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    fn web(port: u16) -> Entry {
        Entry {
            port,
            listening: true,
            http: true,
            status: 200,
            ..Default::default()
        }
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// Feeds `ports` to the worker and returns once it has drained them all.
    fn drain(shared: Arc<Shared>, ports: &[u16]) {
        let (tx, rx) = mpsc::channel();
        for p in ports {
            tx.send(*p).unwrap();
        }
        drop(tx);
        run(shared, rx);
    }

    #[test]
    fn worker_exits_without_a_browser() {
        let h = harness(1, None);
        let (_tx, rx) = mpsc::channel();
        run(h.shared.clone(), rx);
    }

    /// A valid 1x1 RGB PNG, so the real `sips` conversion path runs on macOS.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xc9, 0xfe, 0x92, 0xef, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// Shell snippet that copies `png` to the path given in `--screenshot=`.
    fn write_screenshot(png: &Path) -> String {
        format!(
            "for a in \"$@\"; do case \"$a\" in --screenshot=*) cp '{}' \"${{a#--screenshot=}}\";; esac; done",
            png.display()
        )
    }

    fn wait_for(path: &Path) {
        let start = Instant::now();
        while !path.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out waiting for {path:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    // All captures live in one test: `capture` uses a per-process scratch directory.
    #[test]
    fn captures_with_a_headless_browser_and_records_failures() {
        let bin = TempDir::new();
        let png = bin.path().join("fixture.png");
        fs::write(&png, PNG).unwrap();
        let args_log = bin.path().join("args");
        let ok = script(
            bin.path(),
            "ok-browser",
            &format!(
                "echo \"$@\" > '{}'\n{}",
                args_log.display(),
                write_screenshot(&png)
            ),
        );
        let h = harness(1, Some(ok));
        {
            let mut store = h.shared.store.lock().unwrap();
            store.entries.insert(3000, web(3000));
            store.thumb_queue.insert(3000);
        }
        drain(h.shared.clone(), &[3000]);

        let args = fs::read_to_string(&args_log).unwrap();
        assert!(args.contains("--headless=new"));
        assert!(args.trim_end().ends_with("http://localhost:3000/"));
        let thumb = fs::read(Store::thumb_path(h.dir.path(), 3000)).unwrap();
        if cfg!(target_os = "macos") {
            assert!(
                thumb.starts_with(&[0xff, 0xd8, 0xff]),
                "sips produced a JPEG"
            );
        } else {
            assert_eq!(thumb, PNG, "without sips the PNG is kept as-is");
        }
        {
            let store = h.shared.store.lock().unwrap();
            let e = &store.entries[&3000];
            assert!(e.thumb_at > 0);
            assert_eq!(e.thumb_failed_at, 0);
            assert!(store.thumb_queue.is_empty());
            assert!(store.view_json.contains("/thumb/3000?v="));
        }

        let broken = script(bin.path(), "broken-browser", "exit 1");
        let h = harness(1, Some(broken));
        h.shared
            .store
            .lock()
            .unwrap()
            .entries
            .insert(3001, web(3001));
        drain(h.shared.clone(), &[3001]);
        {
            let store = h.shared.store.lock().unwrap();
            let e = &store.entries[&3001];
            assert_eq!(e.thumb_at, 0);
            assert!(e.thumb_failed_at > 0, "failure starts the retry backoff");
            assert!(!Store::thumb_path(h.dir.path(), 3001).exists());
        }

        // The user hides the service while its screenshot is being taken.
        let (started, go) = (bin.path().join("started"), bin.path().join("go"));
        let slow = script(
            bin.path(),
            "slow-browser",
            &format!(
                "touch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\n{}",
                started.display(),
                go.display(),
                write_screenshot(&png)
            ),
        );
        let h = harness(1, Some(slow));
        h.shared
            .store
            .lock()
            .unwrap()
            .entries
            .insert(3002, web(3002));
        let shared = h.shared.clone();
        let worker = thread::spawn(move || drain(shared, &[3002]));
        wait_for(&started);
        {
            let mut store = h.shared.store.lock().unwrap();
            store.entries.get_mut(&3002).unwrap().hidden = true;
            store.drop_thumb(3002);
        }
        fs::write(&go, "").unwrap();
        worker.join().unwrap();
        assert!(!Store::thumb_path(h.dir.path(), 3002).exists());
        let store = h.shared.store.lock().unwrap();
        let e = &store.entries[&3002];
        assert_eq!((e.thumb_at, e.thumb_failed_at), (0, 0));
    }

    #[test]
    fn skips_work_when_thumbnails_are_off_hidden_or_not_a_card() {
        let bin = TempDir::new();
        let marker = bin.path().join("ran");
        let browser = script(
            bin.path(),
            "browser",
            &format!("touch '{}'", marker.display()),
        );
        let h = harness(1, Some(browser));
        {
            let mut store = h.shared.store.lock().unwrap();
            store.thumbs_wanted = false;
            store.entries.insert(3000, web(3000));
            store.thumb_queue.insert(3000);
        }
        drain(h.shared.clone(), &[3000]);
        assert!(h.shared.store.lock().unwrap().thumb_queue.is_empty());

        {
            let mut store = h.shared.store.lock().unwrap();
            store.thumbs_wanted = true;
            store.entries.insert(
                3001,
                Entry {
                    listening: false,
                    ..web(3001)
                },
            );
        }
        drain(h.shared.clone(), &[3001]);

        h.shared.store.lock().unwrap().entries.insert(
            3002,
            Entry {
                hidden: true,
                ..web(3002)
            },
        );
        drain(h.shared.clone(), &[3002]);
        assert!(!marker.exists(), "browser never launched");
        assert_eq!(
            h.shared.store.lock().unwrap().entries[&3001].thumb_failed_at,
            0,
            "stopped services do not enter backoff"
        );
    }

    #[test]
    fn forgotten_entries_leave_no_thumbnail_behind() {
        let bin = TempDir::new();
        let h = harness(1, Some(script(bin.path(), "browser", "exit 0")));
        let stale = Store::thumb_path(h.dir.path(), 4000);
        fs::write(&stale, "old").unwrap();
        drain(h.shared.clone(), &[4000]);
        assert!(!stale.exists());
    }

    #[test]
    fn shrink_fails_without_a_screenshot() {
        let tmp = TempDir::new();
        assert!(!shrink(
            &tmp.path().join("missing.png"),
            &tmp.path().join("out.jpg")
        ));
    }
}
