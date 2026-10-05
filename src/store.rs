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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use serde_json::Value;
    use std::time::Duration;

    fn web(port: u16) -> Entry {
        Entry {
            port,
            listening: true,
            http: true,
            status: 200,
            ..Default::default()
        }
    }

    fn view(store: &Store) -> Vec<Value> {
        serde_json::from_str(&store.view_json).unwrap()
    }

    fn touch_thumb(dir: &Path, port: u16) -> PathBuf {
        let p = Store::thumb_path(dir, port);
        fs::write(&p, b"jpg").unwrap();
        p
    }

    #[test]
    fn service_classification() {
        assert!(web(1).is_service());
        let api_404 = Entry {
            status: 404,
            ..web(1)
        };
        assert!(!api_404.is_service(), "4xx without a project is not a page");
        assert!(Entry {
            project: Some("api".into()),
            ..api_404.clone()
        }
        .is_service());
        assert!(!Entry {
            http: false,
            ..web(1)
        }
        .is_service());
        assert!(!Entry {
            app: true,
            project: Some("x".into()),
            ..web(1)
        }
        .is_service());
    }

    #[test]
    fn lifecycle_states() {
        assert_eq!(web(1).state(), "live");
        let crashed = Entry {
            status: 500,
            project: Some("x".into()),
            ..web(1)
        };
        assert_eq!(crashed.state(), "error");
        assert_eq!(
            Entry {
                http: false,
                ..web(1)
            }
            .state(),
            "other"
        );
        let gone = Entry {
            listening: false,
            ..web(1)
        };
        assert_eq!(gone.state(), "stale");
        assert_eq!(
            Entry {
                pinned: true,
                ..gone
            }
            .state(),
            "offline"
        );
    }

    #[test]
    fn cards_and_urls() {
        assert!(web(1).shows_card());
        let pinned_api = Entry {
            status: 404,
            pinned: true,
            ..web(1)
        };
        assert!(pinned_api.shows_card(), "pinned HTTP listeners get a card");
        assert!(!Entry {
            listening: false,
            ..web(1)
        }
        .shows_card());
        assert!(!Entry {
            http: false,
            pinned: true,
            ..web(1)
        }
        .shows_card());

        assert_eq!(web(3000).url(), "http://localhost:3000/");
        let lan = Entry {
            link_host: "192.168.1.5".into(),
            ..web(8080)
        };
        assert_eq!(lan.url(), "http://192.168.1.5:8080/");
    }

    #[test]
    fn save_and_load_round_trip_without_transient_fields() {
        let tmp = TempDir::new();
        let mut store = Store::load(tmp.path());
        assert!(store.entries.is_empty());
        store.entries.insert(
            3000,
            Entry {
                label: Some("Docs".into()),
                pinned: true,
                pin_order: 2,
                pid: 4242,
                probed_at: 99,
                thumb_failed_at: 7,
                ..web(3000)
            },
        );
        store.save();
        assert!(tmp.path().join("state.json").exists());
        assert!(!tmp.path().join("state.json.tmp").exists());

        let loaded = Store::load(tmp.path());
        let e = &loaded.entries[&3000];
        assert_eq!(e.label.as_deref(), Some("Docs"));
        assert!(e.pinned && e.http);
        assert_eq!((e.pin_order, e.status), (2, 200));
        // Runtime-only facts are rediscovered by the next scan, never persisted.
        assert!(!e.listening);
        assert_eq!((e.pid, e.probed_at, e.thumb_failed_at), (0, 0, 0));
    }

    #[test]
    fn load_tolerates_missing_corrupt_and_partial_state() {
        let tmp = TempDir::new();
        fs::write(tmp.path().join("state.json"), "{not json").unwrap();
        assert!(Store::load(tmp.path()).entries.is_empty());

        fs::write(
            tmp.path().join("state.json"),
            r#"{"4000":{"port":4000,"pinned":true}}"#,
        )
        .unwrap();
        let store = Store::load(tmp.path());
        let e = &store.entries[&4000];
        assert!(e.pinned);
        assert_eq!(e.label, None);
        assert_eq!(store.version, 1, "load renders the initial view");

        let missing = tmp.path().join("nope");
        assert!(Store::load(&missing).entries.is_empty());
    }

    #[test]
    fn load_sweeps_orphan_thumbnails() {
        let tmp = TempDir::new();
        let mut store = Store::load(tmp.path());
        store.entries.insert(
            3000,
            Entry {
                thumb_at: 5,
                ..web(3000)
            },
        );
        store.entries.insert(3001, web(3001));
        store.save();
        let keep = touch_thumb(tmp.path(), 3000);
        let never_captured = touch_thumb(tmp.path(), 3001);
        let unknown = touch_thumb(tmp.path(), 3002);
        let junk = tmp.path().join("thumbs/notes.txt");
        fs::write(&junk, "x").unwrap();

        Store::load(tmp.path());
        assert!(keep.exists());
        assert!(!never_captured.exists());
        assert!(!unknown.exists());
        assert!(!junk.exists());
    }

    #[test]
    fn expire_rules() {
        let tmp = TempDir::new();
        let mut store = Store::load(tmp.path());
        let t = now();
        let gone = |port, last_seen| Entry {
            listening: false,
            last_seen,
            ..web(port)
        };
        store.entries.insert(1, gone(1, t - 1000));
        store.entries.insert(2, gone(2, t - 10));
        store.entries.insert(
            3,
            Entry {
                pinned: true,
                ..gone(3, 0)
            },
        );
        store.entries.insert(
            4,
            Entry {
                hidden: true,
                ..gone(4, 0)
            },
        );
        store.entries.insert(5, web(5));
        store.entries.insert(
            6,
            Entry {
                http: false,
                ..gone(6, t)
            },
        );
        let thumb = touch_thumb(tmp.path(), 1);

        assert!(store.expire(600));
        let left: Vec<u16> = store.entries.keys().copied().collect();
        // Old stale service and a stopped non-service go; recent, pinned, hidden, live stay.
        assert_eq!(left, vec![2, 3, 4, 5]);
        assert!(!thumb.exists());

        assert!(!store.expire(600), "nothing more to expire");
        assert!(store.expire(0), "ttl 0 clears every stale entry");
        assert_eq!(
            store.entries.keys().copied().collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    #[test]
    fn forget_and_drop_thumb() {
        let tmp = TempDir::new();
        let mut store = Store::load(tmp.path());
        store.entries.insert(
            1,
            Entry {
                thumb_at: 9,
                ..web(1)
            },
        );
        store.entries.insert(2, web(2));
        let t1 = touch_thumb(tmp.path(), 1);
        let t2 = touch_thumb(tmp.path(), 2);

        store.drop_thumb(1);
        assert_eq!(store.entries[&1].thumb_at, 0);
        assert!(!t1.exists());

        store.forget(2);
        assert!(!store.entries.contains_key(&2));
        assert!(!t2.exists());
        store.forget(9999);
        store.drop_thumb(9999);
    }

    #[test]
    fn view_version_only_moves_on_visible_change() {
        let tmp = TempDir::new();
        let mut store = Store::load(tmp.path());
        let v0 = store.version;
        store.refresh_view();
        assert_eq!(store.version, v0);

        store.entries.insert(3000, web(3000));
        store.refresh_view();
        assert_eq!(store.version, v0 + 1);

        // Fields clients never see do not cause churn.
        let e = store.entries.get_mut(&3000).unwrap();
        e.pid = 1;
        e.probed_at = 123;
        e.last_seen = 456;
        store.refresh_view();
        assert_eq!(store.version, v0 + 1);

        store.entries.get_mut(&3000).unwrap().title = Some("New".into());
        store.refresh_view();
        assert_eq!(store.version, v0 + 2);
    }

    #[test]
    fn view_name_falls_back_label_project_title_runtime_port() {
        let tmp = TempDir::new();
        let mut store = Store::load(tmp.path());
        let full = Entry {
            label: Some("Label".into()),
            project: Some("proj".into()),
            title: Some("Title".into()),
            runtime: Some("vite".into()),
            ..web(1)
        };
        store.entries.insert(1, full.clone());
        store.entries.insert(
            2,
            Entry {
                port: 2,
                label: None,
                ..full.clone()
            },
        );
        store.entries.insert(
            3,
            Entry {
                port: 3,
                label: None,
                project: None,
                ..full.clone()
            },
        );
        store.entries.insert(
            4,
            Entry {
                port: 4,
                label: None,
                project: None,
                title: None,
                ..full.clone()
            },
        );
        store.entries.insert(5, web(5));
        store.refresh_view();
        let names: Vec<String> = view(&store)
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["Label", "proj", "Title", "vite", ":5"]);
    }

    #[test]
    fn view_fields() {
        let tmp = TempDir::new();
        let mut store = Store::load(tmp.path());
        let home = std::env::var("HOME").unwrap_or_default();
        store.entries.insert(
            1,
            Entry {
                thumb_at: 77,
                cwd: Some(format!("{home}/code/app")),
                ..web(1)
            },
        );
        store.entries.insert(
            2,
            Entry {
                listening: false,
                last_seen: 1234,
                cwd: Some("/nonexistent-portmap-test/srv".into()),
                ..web(2)
            },
        );
        store.refresh_view();
        let v = view(&store);
        assert_eq!(v[0]["url"], "http://localhost:1/");
        assert_eq!(v[0]["state"], "live");
        assert_eq!(v[0]["thumb"], "/thumb/1?v=77");
        assert!(v[0]["last_seen"].is_null(), "live entries omit last_seen");
        if !home.is_empty() {
            assert_eq!(v[0]["cwd"], "~/code/app");
        }
        assert_eq!(v[1]["state"], "stale");
        assert_eq!(v[1]["last_seen"], 1234);
        assert!(v[1]["thumb"].is_null());
        assert_eq!(v[1]["cwd"], "/nonexistent-portmap-test/srv");
    }

    #[test]
    fn client_activity_window() {
        let tmp = TempDir::new();
        let mut store = Store::load(tmp.path());
        assert!(store.client_active());
        if let Some(earlier) = Instant::now().checked_sub(Duration::from_secs(30)) {
            store.last_client = earlier;
            assert!(!store.client_active());
        }
    }
}
