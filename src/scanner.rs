//! Background scan loop. Cadence adapts to whether anyone is looking: every few seconds
//! while a browser tab is polling, rarely otherwise. HTTP probes are cached per
//! (port, pid) so dev servers are not spammed with requests every scan.

use crate::scan;
use crate::store::{now, Entry};
use crate::Shared;
use std::collections::HashMap;
use std::path::PathBuf;
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
        cycle(&shared, force);
        last_scan = Instant::now();
        for w in waiters {
            let _ = w.send(());
        }
    }
}

pub fn cycle(shared: &Shared, force: bool) {
    let me = std::process::id();
    let mut listeners: Vec<scan::Listener> = scan::listeners()
        .into_iter()
        .filter(|l| l.port > 1024 && l.port != shared.cfg.port && l.pid != me)
        .collect();
    let pids: Vec<u32> = {
        let mut p: Vec<u32> = listeners.iter().map(|l| l.pid).collect();
        p.sort_unstable();
        p.dedup();
        p
    };
    let args = scan::process_args(&pids);
    listeners.retain(|l| args.get(&l.pid).is_none_or(|a| !scan::is_system_process(a)));
    let cwds = scan::process_cwds(&pids);
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());

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
        e.project = cwd.and_then(|c| scan::project_name(c, &home));
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
