//! Background scan loop. Cadence adapts to whether anyone is looking: every few seconds
//! while a browser tab is polling, rarely otherwise. HTTP probes are cached per
//! (port, pid) so dev servers are not spammed with requests every scan.

use crate::scan;
use crate::store::{now, Entry};
use crate::Shared;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub enum ScanMsg {
    /// Rescan now; `force` re-probes every port and refreshes old thumbnails.
    Refresh {
        force: bool,
        done: Option<Sender<()>>,
    },
}

const ACTIVE_INTERVAL: Duration = Duration::from_secs(3);
const IDLE_INTERVAL: Duration = Duration::from_secs(30);
const REPROBE_SECS: u64 = 60;
const THUMB_MAX_AGE_SECS: u64 = 15 * 60;
const THUMB_RETRY_SECS: u64 = 10 * 60;

pub fn run(shared: Arc<Shared>, rx: Receiver<ScanMsg>) {
    run_with(&shared, rx, cycle)
}

/// The scheduling loop, with the scan itself injectable so tests can observe how
/// refresh requests are batched without touching the real machine.
fn run_with(shared: &Shared, rx: Receiver<ScanMsg>, mut scan: impl FnMut(&Shared, bool)) {
    let mut last_scan = Instant::now();
    loop {
        let mut force = false;
        let mut waiters = Vec::new();
        // Tick at the active cadence so a tab that opens mid-wait is served promptly;
        // when nobody is watching, only every IDLE_INTERVAL does a tick turn into a scan.
        match rx.recv_timeout(ACTIVE_INTERVAL) {
            Ok(ScanMsg::Refresh { force: f, done }) => {
                force = f;
                waiters.extend(done);
            }
            Err(RecvTimeoutError::Timeout) => {
                let active = shared.store.lock().unwrap().client_active();
                if !active && last_scan.elapsed() < IDLE_INTERVAL {
                    continue;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(ScanMsg::Refresh { force: f, done }) = rx.try_recv() {
            force |= f;
            waiters.extend(done);
        }
        scan(shared, force);
        last_scan = Instant::now();
        for w in waiters {
            let _ = w.send(());
        }
    }
}

pub fn cycle(shared: &Shared, force: bool) {
    let listeners = relevant(shared, scan::listeners());
    let pids: Vec<u32> = {
        let mut p: Vec<u32> = listeners.iter().map(|l| l.pid).collect();
        p.sort_unstable();
        p.dedup();
        p
    };
    let args = scan::process_args(&pids);
    let cwds = scan::process_cwds(&pids);
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    update(shared, listeners, &args, &cwds, &home, force);
}

/// Privileged ports, portmap's own UI port, and portmap itself are never bookmarks.
fn relevant(shared: &Shared, listeners: Vec<scan::Listener>) -> Vec<scan::Listener> {
    let me = std::process::id();
    listeners
        .into_iter()
        .filter(|l| l.port > 1024 && l.port != shared.cfg.port && l.pid != me)
        .collect()
}

/// Merges one round of discovery into the store: probes what changed, marks vanished
/// ports, expires stale entries, and queues thumbnails.
fn update(
    shared: &Shared,
    mut listeners: Vec<scan::Listener>,
    args: &HashMap<u32, String>,
    cwds: &HashMap<u32, PathBuf>,
    home: &Path,
    force: bool,
) {
    listeners.retain(|l| args.get(&l.pid).is_none_or(|a| !scan::is_system_process(a)));

    // Decide what needs probing while holding the lock only briefly.
    let to_probe: Vec<&scan::Listener> = {
        let store = shared.store.lock().unwrap();
        let active = store.client_active();
        let t = now();
        listeners
            .iter()
            .filter(|l| match store.entries.get(&l.port) {
                None => true,
                Some(e) => {
                    force
                        || !e.listening
                        || e.pid != l.pid
                        || e.probed_at == 0
                        || (active && t.saturating_sub(e.probed_at) >= REPROBE_SECS)
                }
            })
            .collect()
    };
    let probes: HashMap<u16, scan::Probe> = thread::scope(|s| {
        let handles: Vec<_> = to_probe
            .iter()
            .map(|l| (l.port, s.spawn(|| scan::probe(l.port, &l.addrs))))
            .collect();
        handles
            .into_iter()
            .filter_map(|(p, h)| h.join().ok().map(|r| (p, r)))
            .collect()
    });

    let mut store = shared.store.lock().unwrap();
    let t = now();
    let active = store.client_active();
    let thumbs_wanted = store.thumbs_wanted;
    let mut thumbs = Vec::new();
    let mut seen = Vec::with_capacity(listeners.len());
    for l in &listeners {
        seen.push(l.port);
        let argv = args.get(&l.pid).map(String::as_str).unwrap_or("");
        let cwd = cwds.get(&l.pid).filter(|c| c.as_os_str() != "/");
        let e = store.entries.entry(l.port).or_insert_with(|| Entry {
            port: l.port,
            first_seen: t,
            ..Default::default()
        });
        let restarted = e.pid != l.pid;
        let old_title = e.title.clone();
        if !e.listening && e.last_seen > 0 && t.saturating_sub(e.last_seen) > 3600 && !e.pinned {
            // Same port, long gone: treat it as a new service.
            e.first_seen = t;
        }
        e.listening = true;
        e.pid = l.pid;
        e.last_seen = t;
        e.runtime = Some(scan::runtime_label(argv, &l.command));
        e.app = scan::is_desktop_app(argv);
        e.cwd = cwd.map(|c| c.to_string_lossy().to_string());
        e.project = cwd.and_then(|c| scan::project_name(c, home));
        if let Some(p) = probes.get(&l.port) {
            e.http = p.http;
            e.status = p.status;
            e.title = p.title.clone();
            e.link_host = p.link_host.clone();
            e.probed_at = t;
        }
        let want_thumb = e.shows_card()
            && !e.hidden
            && shared.cfg.browser.is_some()
            && thumbs_wanted
            && (e.thumb_at == 0
                || restarted
                || e.title != old_title
                || force
                || (active && t.saturating_sub(e.thumb_at) >= THUMB_MAX_AGE_SECS))
            && (force || t.saturating_sub(e.thumb_failed_at) >= THUMB_RETRY_SECS);
        if want_thumb {
            thumbs.push(l.port);
        }
    }
    for e in store.entries.values_mut() {
        if !seen.contains(&e.port) {
            e.listening = false;
            e.pid = 0;
        }
    }
    store.expire(shared.cfg.stale_secs);
    store.scanned_at = t;
    let before = store.version;
    store.refresh_view();
    if store.version != before {
        store.save();
    }
    for port in thumbs {
        if store.thumb_queue.insert(port) {
            let _ = shared.thumb_tx.send(port);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{closed_port, harness, html, http_server, TempDir};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    fn listener(port: u16, pid: u32) -> scan::Listener {
        scan::Listener {
            port,
            pid,
            command: "node".into(),
            addrs: vec!["127.0.0.1".into()],
        }
    }

    fn argv(pairs: &[(u32, &str)]) -> HashMap<u32, String> {
        pairs.iter().map(|(p, a)| (*p, a.to_string())).collect()
    }

    fn scan_once(shared: &Shared, ls: Vec<scan::Listener>, force: bool) {
        update(
            shared,
            ls,
            &HashMap::new(),
            &HashMap::new(),
            Path::new(""),
            force,
        );
    }

    #[test]
    fn relevant_drops_privileged_own_and_self() {
        let h = harness(7878, None);
        let me = std::process::id();
        let kept = relevant(
            &h.shared,
            vec![
                listener(80, 1),
                listener(1024, 1),
                listener(7878, 2),
                listener(5000, me),
                listener(3000, 3),
            ],
        );
        assert_eq!(kept.iter().map(|l| l.port).collect::<Vec<_>>(), vec![3000]);
    }

    #[test]
    fn new_http_listener_becomes_a_live_entry() {
        let (port, _) = http_server(|_| html("200 OK", "My App"));
        let h = harness(1, None);
        let home = TempDir::new();
        let app = home.path().join("myapp");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("package.json"), "{}").unwrap();

        update(
            &h.shared,
            vec![listener(port, 4242)],
            &argv(&[(4242, "node /x/node_modules/.bin/vite --port 5173")]),
            &[(4242, app.clone())].into_iter().collect(),
            home.path(),
            false,
        );

        let store = h.shared.store.lock().unwrap();
        let e = &store.entries[&port];
        assert!(e.listening && e.http);
        assert_eq!((e.pid, e.status), (4242, 200));
        assert_eq!(e.title.as_deref(), Some("My App"));
        assert_eq!(e.runtime.as_deref(), Some("vite"));
        assert_eq!(e.project.as_deref(), Some("myapp"));
        assert_eq!(e.cwd.as_deref(), Some(app.to_str().unwrap()));
        assert_eq!(e.state(), "live");
        assert!(e.first_seen > 0 && e.probed_at > 0);
        assert!(store.scanned_at > 0);
        assert!(
            h.dir.path().join("state.json").exists(),
            "changes are saved"
        );
        assert!(h.thumb_rx.try_recv().is_err(), "no browser, no thumbnails");
    }

    #[test]
    fn system_processes_and_root_cwds_are_ignored() {
        let h = harness(1, None);
        let (port, _) = http_server(|_| html("200 OK", "x"));
        let closed = closed_port();
        update(
            &h.shared,
            vec![listener(closed, 10), listener(port, 11)],
            &argv(&[(10, "/usr/libexec/rapportd")]),
            &[(11, PathBuf::from("/"))].into_iter().collect(),
            Path::new("/"),
            false,
        );
        let store = h.shared.store.lock().unwrap();
        assert!(!store.entries.contains_key(&closed));
        let e = &store.entries[&port];
        assert_eq!((e.cwd.as_deref(), e.project.as_deref()), (None, None));
        assert_eq!(
            e.runtime.as_deref(),
            Some("node"),
            "falls back to lsof's command"
        );
    }

    #[test]
    fn probes_are_cached_until_restart_return_or_force() {
        let (port, hits) = http_server(|_| html("200 OK", "cached"));
        let h = harness(1, None);
        let hits = || hits.load(Ordering::SeqCst);

        scan_once(&h.shared, vec![listener(port, 1)], false);
        assert_eq!(hits(), 1);
        scan_once(&h.shared, vec![listener(port, 1)], false);
        assert_eq!(hits(), 1, "same pid, recently probed: no request");
        scan_once(&h.shared, vec![listener(port, 2)], false);
        assert_eq!(hits(), 2, "new pid means a restarted server");
        scan_once(&h.shared, vec![listener(port, 2)], true);
        assert_eq!(hits(), 3, "force re-probes");
        scan_once(&h.shared, vec![], false);
        scan_once(&h.shared, vec![listener(port, 2)], false);
        assert_eq!(hits(), 4, "a port that comes back is re-probed");
    }

    #[test]
    fn vanished_ports_go_stale_and_non_services_drop() {
        let (web, _) = http_server(|_| html("200 OK", "web"));
        let (ssh, _) = http_server(|_| b"SSH-2.0-x\r\n".to_vec());
        let h = harness(1, None);
        scan_once(&h.shared, vec![listener(web, 1), listener(ssh, 2)], false);
        {
            let store = h.shared.store.lock().unwrap();
            assert_eq!(store.entries[&web].state(), "live");
            assert_eq!(store.entries[&ssh].state(), "other");
        }
        let v = h.shared.store.lock().unwrap().version;
        scan_once(&h.shared, vec![], false);
        let store = h.shared.store.lock().unwrap();
        let e = &store.entries[&web];
        assert_eq!((e.listening, e.pid, e.state()), (false, 0, "stale"));
        assert!(!store.entries.contains_key(&ssh));
        assert!(store.version > v);
    }

    #[test]
    fn long_gone_port_counts_as_a_new_service_unless_pinned() {
        let (port, _) = http_server(|_| html("200 OK", "back"));
        let (pinned_port, _) = http_server(|_| html("200 OK", "pinned"));
        let h = harness(1, None);
        let old = now() - 7200;
        {
            let mut store = h.shared.store.lock().unwrap();
            for (p, pinned) in [(port, false), (pinned_port, true)] {
                store.entries.insert(
                    p,
                    Entry {
                        port: p,
                        first_seen: 1,
                        last_seen: old,
                        pinned,
                        ..Default::default()
                    },
                );
            }
        }
        scan_once(
            &h.shared,
            vec![listener(port, 1), listener(pinned_port, 2)],
            false,
        );
        let store = h.shared.store.lock().unwrap();
        assert!(store.entries[&port].first_seen > old);
        assert_eq!(store.entries[&pinned_port].first_seen, 1);
    }

    #[test]
    fn thumbnails_are_queued_once_when_wanted() {
        let (port, _) = http_server(|_| html("200 OK", "shot"));
        let h = harness(1, Some(PathBuf::from("/fake/browser")));
        scan_once(&h.shared, vec![listener(port, 1)], false);
        assert_eq!(h.thumb_rx.try_recv(), Ok(port));
        assert!(h.shared.store.lock().unwrap().thumb_queue.contains(&port));

        scan_once(&h.shared, vec![listener(port, 1)], false);
        assert!(h.thumb_rx.try_recv().is_err(), "already queued");
    }

    #[test]
    fn thumbnails_respect_preference_hidden_and_backoff() {
        let (port, _) = http_server(|_| html("200 OK", "shot"));
        let browser = Some(PathBuf::from("/fake/browser"));

        let h = harness(1, browser.clone());
        h.shared.store.lock().unwrap().thumbs_wanted = false;
        scan_once(&h.shared, vec![listener(port, 1)], false);
        assert!(h.thumb_rx.try_recv().is_err(), "thumbnails switched off");

        let h = harness(1, browser.clone());
        h.shared.store.lock().unwrap().entries.insert(
            port,
            Entry {
                port,
                hidden: true,
                ..Default::default()
            },
        );
        scan_once(&h.shared, vec![listener(port, 1)], false);
        assert!(
            h.thumb_rx.try_recv().is_err(),
            "hidden entries get no thumbnail"
        );

        let h = harness(1, browser);
        scan_once(&h.shared, vec![listener(port, 1)], false);
        assert_eq!(h.thumb_rx.try_recv(), Ok(port));
        {
            let mut store = h.shared.store.lock().unwrap();
            store.thumb_queue.clear();
            store.entries.get_mut(&port).unwrap().thumb_failed_at = now();
        }
        scan_once(&h.shared, vec![listener(port, 1)], false);
        assert!(h.thumb_rx.try_recv().is_err(), "recent failure backs off");
        scan_once(&h.shared, vec![listener(port, 1)], true);
        assert_eq!(h.thumb_rx.try_recv(), Ok(port), "force retries anyway");
    }

    #[test]
    fn run_batches_pending_refreshes_into_one_scan() {
        let h = harness(1, None);
        let (tx, rx) = mpsc::channel();
        let (d1, w1) = mpsc::channel();
        let (d2, w2) = mpsc::channel();
        tx.send(ScanMsg::Refresh {
            force: false,
            done: Some(d1),
        })
        .unwrap();
        tx.send(ScanMsg::Refresh {
            force: true,
            done: Some(d2),
        })
        .unwrap();
        tx.send(ScanMsg::Refresh {
            force: false,
            done: None,
        })
        .unwrap();
        drop(tx);

        let mut scans = Vec::new();
        run_with(&h.shared, rx, |_, force| scans.push(force));
        assert_eq!(scans, vec![true], "one forced scan for the whole batch");
        assert_eq!(w1.try_recv(), Ok(()));
        assert_eq!(w2.try_recv(), Ok(()));
    }
}
