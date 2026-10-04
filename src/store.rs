//! The bookmark store: one entry per port, persisted to `state.json`.
//!
//! Lifecycle: a listening HTTP port is `live` (or `error` for 5xx). When it stops
//! listening it becomes `stale` and is removed after the stale TTL, together with its
//! cached thumbnail. Pinned entries never expire; they show as `offline` instead.
//! Non-service listeners are listed as `other` and dropped as soon as they stop.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub port: u16,
    pub label: Option<String>,
    pub pinned: bool,
    /// Position among pinned entries, set by drag and drop in the UI.
    pub pin_order: u32,
    pub hidden: bool,
    pub http: bool,
    pub status: u16,
    pub title: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub runtime: Option<String>,
    pub link_host: String,
    pub first_seen: u64,
    pub last_seen: u64,
    pub thumb_at: u64,
    /// Belongs to a desktop app bundle rather than something the user started.
    pub app: bool,
    #[serde(skip)]
    pub listening: bool,
    #[serde(skip)]
    pub pid: u32,
    #[serde(skip)]
    pub probed_at: u64,
    #[serde(skip)]
    pub thumb_failed_at: u64,
}

impl Entry {
    /// A web page worth a card. Anything else listening (databases, app internals,
    /// APIs without a project that 4xx on `/`) is listed compactly as `other`.
    pub fn is_service(&self) -> bool {
        self.http && !self.app && (self.project.is_some() || self.status < 400)
    }

    /// Worth a thumbnail: a service, or any HTTP listener the user pinned.
    pub fn shows_card(&self) -> bool {
        self.listening && (self.is_service() || (self.pinned && self.http))
    }

    pub fn state(&self) -> &'static str {
        match (self.listening, self.pinned) {
            (true, _) if !self.is_service() => "other",
            (true, _) if self.status >= 500 => "error",
            (true, _) => "live",
            (false, true) => "offline",
            (false, false) => "stale",
        }
    }

    pub fn url(&self) -> String {
        let host = if self.link_host.is_empty() {
            "localhost"
        } else {
            &self.link_host
        };
        format!("http://{host}:{}/", self.port)
    }
}

#[derive(Serialize)]
struct ServiceView<'a> {
    port: u16,
    url: String,
    name: String,
    label: Option<&'a str>,
    title: Option<&'a str>,
    project: Option<&'a str>,
    cwd: Option<String>,
    runtime: Option<&'a str>,
    state: &'static str,
    status: u16,
    pinned: bool,
    pin_order: u32,
    hidden: bool,
    thumb: Option<String>,
    first_seen: u64,
    /// Only for entries that are gone; live entries omit it so the view stays stable
    /// between scans and clients get cheap 304s.
    last_seen: Option<u64>,
}

pub struct Store {
    pub entries: BTreeMap<u16, Entry>,
    pub version: u64,
    pub view_json: String,
    pub scanned_at: u64,
    pub last_client: Instant,
    pub thumb_queue: HashSet<u16>,
    /// Last preference reported by a polling page. While the UI has thumbnails switched
    /// off, no browser is launched at all.
    pub thumbs_wanted: bool,
    dir: PathBuf,
}

impl Store {
    pub fn load(dir: &Path) -> Store {
        let entries: BTreeMap<u16, Entry> = fs::read(dir.join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut store = Store {
            entries,
            version: 0,
            view_json: String::new(),
            scanned_at: 0,
            last_client: Instant::now(),
            thumb_queue: HashSet::new(),
            thumbs_wanted: true,
            dir: dir.to_path_buf(),
        };
        store.sweep_orphan_thumbs();
        store.refresh_view();
        store
    }

    pub fn thumbs_dir(&self) -> PathBuf {
        self.dir.join("thumbs")
    }

    pub fn thumb_path(dir: &Path, port: u16) -> PathBuf {
        dir.join("thumbs").join(format!("{port}.jpg"))
    }

    pub fn save(&self) {
        let Ok(json) = serde_json::to_vec_pretty(&self.entries) else {
            return;
        };
        let tmp = self.dir.join("state.json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(tmp, self.dir.join("state.json"));
        }
    }

    pub fn client_active(&self) -> bool {
        self.last_client.elapsed().as_secs() < 20
    }

    /// Removes an entry and its cached thumbnail.
    pub fn forget(&mut self, port: u16) {
        self.entries.remove(&port);
        let _ = fs::remove_file(Self::thumb_path(&self.dir, port));
    }

    pub fn drop_thumb(&mut self, port: u16) {
        if let Some(e) = self.entries.get_mut(&port) {
            e.thumb_at = 0;
        }
        let _ = fs::remove_file(Self::thumb_path(&self.dir, port));
    }

    /// Expire stale entries. `ttl = 0` clears every stale entry immediately.
    pub fn expire(&mut self, ttl: u64) -> bool {
        let now = now();
        let doomed: Vec<u16> = self
            .entries
            .values()
            .filter(|e| !e.listening && !e.pinned && !e.hidden)
            .filter(|e| !e.is_service() || now.saturating_sub(e.last_seen) >= ttl)
            .map(|e| e.port)
            .collect();
        for port in &doomed {
            self.forget(*port);
        }
        !doomed.is_empty()
    }

    fn sweep_orphan_thumbs(&self) {
        let Ok(dir) = fs::read_dir(self.thumbs_dir()) else {
            return;
        };
        for f in dir.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            let port = name
                .strip_suffix(".jpg")
                .and_then(|p| p.parse::<u16>().ok());
            let keep = port
                .and_then(|p| self.entries.get(&p))
                .is_some_and(|e| e.thumb_at > 0);
            if !keep {
                let _ = fs::remove_file(f.path());
            }
        }
    }

    /// Re-render the public view and bump the version if anything a client can see changed.
    pub fn refresh_view(&mut self) {
        let home = std::env::var("HOME").unwrap_or_default();
        let views: Vec<ServiceView> = self
            .entries
            .values()
            .map(|e| ServiceView {
                port: e.port,
                url: e.url(),
                name: e
                    .label
                    .clone()
                    .or_else(|| e.project.clone())
                    .or_else(|| e.title.clone())
                    .or_else(|| e.runtime.clone())
                    .unwrap_or_else(|| format!(":{}", e.port)),
                label: e.label.as_deref(),
                title: e.title.as_deref(),
                project: e.project.as_deref(),
                cwd: e.cwd.as_deref().map(|c| match c.strip_prefix(&home) {
                    Some(rest) if !home.is_empty() => format!("~{rest}"),
                    _ => c.to_string(),
                }),
                runtime: e.runtime.as_deref(),
                state: e.state(),
                status: e.status,
                pinned: e.pinned,
                pin_order: e.pin_order,
                hidden: e.hidden,
                thumb: (e.thumb_at > 0).then(|| format!("/thumb/{}?v={}", e.port, e.thumb_at)),
                first_seen: e.first_seen,
                last_seen: (!e.listening).then_some(e.last_seen),
            })
            .collect();
        let json = serde_json::to_string(&views).unwrap_or_else(|_| "[]".into());
        if json != self.view_json {
            self.view_json = json;
            self.version += 1;
        }
    }
}
