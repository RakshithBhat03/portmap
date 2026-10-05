//! HTTP surface: static UI, a versioned state endpoint (304 when unchanged), actions,
//! and cached thumbnails. Bound to loopback with a Host-header guard against DNS rebinding.

use crate::scanner::ScanMsg;
use crate::store::Store;
use crate::Shared;
use serde::Deserialize;
use std::io::Read;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response};

const INDEX: &str = include_str!("../assets/index.html");
const STYLE: &str = include_str!("../assets/style.css");
const APP: &str = include_str!("../assets/app.js");
const FAVICON: &str = include_str!("../assets/favicon.svg");

pub fn run(shared: Arc<Shared>) -> Result<(), String> {
    let addr = ("127.0.0.1", shared.cfg.port);
    let server = tiny_http::Server::http(addr)
        .map_err(|e| format!("cannot bind 127.0.0.1:{}: {e}", shared.cfg.port))?;
    eprintln!("portmap listening on http://localhost:{}", shared.cfg.port);
    serve(server, shared);
    Ok(())
}

fn serve(server: tiny_http::Server, shared: Arc<Shared>) {
    for req in server.incoming_requests() {
        let shared = shared.clone();
        thread::spawn(move || handle(req, &shared));
    }
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header")
}

fn send(req: Request, status: u16, ctype: &str, body: Vec<u8>, extra: &[(&str, String)]) {
    let mut resp = Response::from_data(body)
        .with_status_code(status)
        .with_header(header("Content-Type", ctype))
        .with_header(header("X-Content-Type-Options", "nosniff"));
    for (k, v) in extra {
        resp = resp.with_header(header(k, v));
    }
    let _ = req.respond(resp);
}

fn req_header<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

fn host_ok(req: &Request, port: u16) -> bool {
    host_allowed(req_header(req, "Host"), port)
}

fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let allowed = [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
    ];
    host.is_some_and(|h| allowed.iter().any(|a| a == h))
}

fn mutation_ok(req: &Request) -> bool {
    mutation_allowed(
        req_header(req, "Content-Type"),
        req_header(req, "Origin"),
        req_header(req, "Host"),
    )
}

/// Mutations must come from our own page: JSON content type (forces a CORS preflight
/// for cross-origin pages) and, when present, a matching Origin.
fn mutation_allowed(content_type: Option<&str>, origin: Option<&str>, host: Option<&str>) -> bool {
    let json = content_type.is_some_and(|c| c.starts_with("application/json"));
    let origin_ok = match (origin, host) {
        (Some(o), Some(h)) => o == format!("http://{h}"),
        (None, _) => true,
        _ => false,
    };
    json && origin_ok
}

fn state_body(store: &Store, thumbs: bool, stale_minutes: u64) -> Vec<u8> {
    format!(
        r#"{{"version":{},"scanned_at":{},"thumbs":{},"stale_minutes":{},"services":{}}}"#,
        store.version, store.scanned_at, thumbs, stale_minutes, store.view_json
    )
    .into_bytes()
}

fn send_state(req: Request, shared: &Shared) {
    let (body, scanned) = {
        let mut store = shared.store.lock().unwrap();
        store.last_client = Instant::now();
        (
            state_body(
                &store,
                shared.cfg.browser.is_some(),
                shared.cfg.stale_secs / 60,
            ),
            store.scanned_at,
        )
    };
    send(
        req,
        200,
        "application/json",
        body,
        &[
            ("Cache-Control", "no-store".into()),
            ("X-Scanned-At", scanned.to_string()),
        ],
    );
}

#[derive(Deserialize)]
struct Action {
    op: String,
    port: Option<u16>,
    value: Option<serde_json::Value>,
}

fn handle(mut req: Request, shared: &Shared) {
    if !host_ok(&req, shared.cfg.port) {
        return send(req, 403, "text/plain", b"forbidden host".to_vec(), &[]);
    }
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let path = path.to_string();
    let query = query.to_string();
    let no_cache = [("Cache-Control", "no-cache".to_string())];
    match (req.method(), path.as_str()) {
        (Method::Get, "/") => send(
            req,
            200,
            "text/html; charset=utf-8",
            INDEX.into(),
            &no_cache,
        ),
        (Method::Get, "/style.css") => {
            send(req, 200, "text/css; charset=utf-8", STYLE.into(), &no_cache)
        }
        (Method::Get, "/app.js") => send(
            req,
            200,
            "text/javascript; charset=utf-8",
            APP.into(),
            &no_cache,
        ),
        (Method::Get, "/favicon.svg") => send(req, 200, "image/svg+xml", FAVICON.into(), &no_cache),
        (Method::Get, "/api/state") => {
            let param = |key: &str| {
                query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix(key))
                    .map(str::to_string)
            };
            let since: u64 = param("v=").and_then(|v| v.parse().ok()).unwrap_or(0);
            let thumbs = param("thumbs=").map(|t| t != "0");
            let (version, scanned) = {
                let mut store = shared.store.lock().unwrap();
                let was_idle = !store.client_active();
                store.last_client = Instant::now();
                let thumbs_turned_on = thumbs == Some(true) && !store.thumbs_wanted;
                if let Some(t) = thumbs {
                    store.thumbs_wanted = t;
                }
                if was_idle || thumbs_turned_on {
                    // Someone just opened the page: don't make them wait for the idle interval.
                    let _ = shared.scan_tx.send(ScanMsg::Refresh {
                        force: false,
                        done: None,
                    });
                }
                (store.version, store.scanned_at)
            };
            if since == version {
                send(
                    req,
                    304,
                    "application/json",
                    Vec::new(),
                    &[("X-Scanned-At", scanned.to_string())],
                );
            } else {
                send_state(req, shared);
            }
        }
        (Method::Post, "/api/refresh") => {
            if !mutation_ok(&req) {
                return send(req, 403, "text/plain", b"forbidden".to_vec(), &[]);
            }
            let (tx, rx) = mpsc::channel();
            let _ = shared.scan_tx.send(ScanMsg::Refresh {
                force: true,
                done: Some(tx),
            });
            let _ = rx.recv_timeout(Duration::from_secs(8));
            send_state(req, shared);
        }
        (Method::Post, "/api/action") => {
            if !mutation_ok(&req) {
                return send(req, 403, "text/plain", b"forbidden".to_vec(), &[]);
            }
            let mut body = String::new();
            let _ = req.as_reader().take(16 * 1024).read_to_string(&mut body);
            let Ok(action) = serde_json::from_str::<Action>(&body) else {
                return send(req, 400, "text/plain", b"bad request".to_vec(), &[]);
            };
            match apply(shared, action) {
                Ok(()) => send_state(req, shared),
                Err(msg) => send(req, 400, "text/plain", msg.into(), &[]),
            }
        }
        (Method::Get, p) if p.starts_with("/thumb/") => {
            let port = p.trim_start_matches("/thumb/").parse::<u16>().ok();
            match port
                .and_then(|port| std::fs::read(Store::thumb_path(&shared.cfg.data_dir, port)).ok())
            {
                Some(bytes) => send(
                    req,
                    200,
                    "image/jpeg",
                    bytes,
                    &[("Cache-Control", "max-age=31536000, immutable".into())],
                ),
                None => send(req, 404, "text/plain", b"no thumbnail".to_vec(), &[]),
            }
        }
        _ => send(req, 404, "text/plain", b"not found".to_vec(), &[]),
    }
}

fn apply(shared: &Shared, a: Action) -> Result<(), &'static str> {
    let mut store = shared.store.lock().unwrap();
    let flag = a.value.as_ref().and_then(|v| v.as_bool()).unwrap_or(true);
    match a.op.as_str() {
        "clear_stale" => {
            store.expire(0);
        }
        "reorder" => {
            let ports: Vec<u16> = a
                .value
                .as_ref()
                .and_then(|v| v.as_array())
                .ok_or("reorder needs a list of ports")?
                .iter()
                .filter_map(|p| p.as_u64().and_then(|p| u16::try_from(p).ok()))
                .collect();
            for (i, port) in ports.iter().enumerate() {
                if let Some(e) = store.entries.get_mut(port).filter(|e| e.pinned) {
                    e.pin_order = i as u32;
                }
            }
        }
        op => {
            let port = a.port.ok_or("missing port")?;
            if op == "forget" {
                store.forget(port);
            } else {
                let next_pin = store
                    .entries
                    .values()
                    .filter(|e| e.pinned)
                    .map(|e| e.pin_order + 1)
                    .max()
                    .unwrap_or(0);
                let entry = store.entries.get_mut(&port).ok_or("unknown port")?;
                match op {
                    "pin" => {
                        if flag && !entry.pinned {
                            entry.pin_order = next_pin;
                        }
                        entry.pinned = flag;
                    }
                    "hide" => entry.hidden = flag,
                    "rename" => {
                        entry.label = a
                            .value
                            .as_ref()
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().chars().take(80).collect::<String>())
                            .filter(|s| !s.is_empty());
                    }
                    "recapture" => {
                        if shared.cfg.browser.is_none() {
                            return Err("no browser available for thumbnails");
                        }
                        entry.thumb_failed_at = 0;
                        if entry.shows_card() && store.thumb_queue.insert(port) {
                            let _ = shared.thumb_tx.send(port);
                        }
                    }
                    _ => return Err("unknown op"),
                }
                if op == "hide" && flag {
                    store.drop_thumb(port);
                }
            }
        }
    }
    store.refresh_view();
    store.save();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Entry;
    use crate::testutil::{harness, Harness};
    use serde_json::{json, Value};
    use std::io::Write;
    use std::net::TcpStream;
    use std::path::PathBuf;

    fn web(port: u16) -> Entry {
        Entry {
            port,
            listening: true,
            http: true,
            status: 200,
            ..Default::default()
        }
    }

    fn act(h: &Harness, a: Value) -> Result<(), &'static str> {
        apply(&h.shared, serde_json::from_value(a).unwrap())
    }

    fn entry(h: &Harness, port: u16) -> Option<Entry> {
        h.shared.store.lock().unwrap().entries.get(&port).cloned()
    }

    fn with_entries(browser: Option<PathBuf>, entries: Vec<Entry>) -> Harness {
        let h = harness(1, browser);
        {
            let mut store = h.shared.store.lock().unwrap();
            for e in entries {
                store.entries.insert(e.port, e);
            }
            store.refresh_view();
        }
        h
    }

    #[test]
    fn host_guard_blocks_dns_rebinding() {
        assert!(host_allowed(Some("localhost:7878"), 7878));
        assert!(host_allowed(Some("127.0.0.1:7878"), 7878));
        assert!(host_allowed(Some("[::1]:7878"), 7878));
        assert!(!host_allowed(Some("localhost:7879"), 7878));
        assert!(!host_allowed(Some("evil.example:7878"), 7878));
        assert!(!host_allowed(Some("localhost"), 7878));
        assert!(!host_allowed(None, 7878));
    }

    #[test]
    fn mutation_guard_requires_json_and_same_origin() {
        let json = Some("application/json");
        let host = Some("localhost:7878");
        assert!(mutation_allowed(json, None, host));
        assert!(mutation_allowed(
            Some("application/json; charset=utf-8"),
            Some("http://localhost:7878"),
            host
        ));
        assert!(!mutation_allowed(Some("text/plain"), None, host));
        assert!(!mutation_allowed(None, None, host));
        assert!(!mutation_allowed(json, Some("http://evil.example"), host));
        assert!(!mutation_allowed(
            json,
            Some("https://localhost:7878"),
            host
        ));
        assert!(!mutation_allowed(json, Some("http://localhost:7878"), None));
    }

    #[test]
    fn pin_assigns_order_and_unpin_clears() {
        let h = with_entries(None, vec![web(1), web(2), web(3)]);
        act(&h, json!({"op": "pin", "port": 2})).unwrap();
        act(&h, json!({"op": "pin", "port": 3})).unwrap();
        assert_eq!(entry(&h, 2).unwrap().pin_order, 0);
        assert_eq!(entry(&h, 3).unwrap().pin_order, 1);
        act(&h, json!({"op": "pin", "port": 2, "value": true})).unwrap();
        assert_eq!(entry(&h, 2).unwrap().pin_order, 0, "re-pinning keeps order");
        act(&h, json!({"op": "pin", "port": 3, "value": false})).unwrap();
        assert!(!entry(&h, 3).unwrap().pinned);
        assert!(h.dir.path().join("state.json").exists());
    }

    #[test]
    fn reorder_only_moves_pinned_entries() {
        let pinned = |p| Entry {
            pinned: true,
            pin_order: 9,
            ..web(p)
        };
        let h = with_entries(None, vec![pinned(1), pinned(2), web(3)]);
        act(
            &h,
            json!({"op": "reorder", "value": [2, "junk", 1, 70000, 3]}),
        )
        .unwrap();
        assert_eq!(entry(&h, 2).unwrap().pin_order, 0);
        assert_eq!(entry(&h, 1).unwrap().pin_order, 1);
        assert_eq!(entry(&h, 3).unwrap().pin_order, 0, "unpinned untouched");
        assert_eq!(
            act(&h, json!({"op": "reorder", "value": 5})),
            Err("reorder needs a list of ports")
        );
    }

    #[test]
    fn hide_drops_the_thumbnail() {
        let h = with_entries(
            None,
            vec![Entry {
                thumb_at: 5,
                ..web(1)
            }],
        );
        let thumb = Store::thumb_path(h.dir.path(), 1);
        std::fs::write(&thumb, "jpg").unwrap();
        act(&h, json!({"op": "hide", "port": 1, "value": false})).unwrap();
        assert!(thumb.exists());
        act(&h, json!({"op": "hide", "port": 1})).unwrap();
        let e = entry(&h, 1).unwrap();
        assert!(e.hidden);
        assert_eq!(e.thumb_at, 0);
        assert!(!thumb.exists());
    }

    #[test]
    fn rename_trims_caps_and_clears() {
        let h = with_entries(None, vec![web(1)]);
        act(&h, json!({"op": "rename", "port": 1, "value": "  Docs  "})).unwrap();
        assert_eq!(entry(&h, 1).unwrap().label.as_deref(), Some("Docs"));
        act(
            &h,
            json!({"op": "rename", "port": 1, "value": "é".repeat(200)}),
        )
        .unwrap();
        assert_eq!(entry(&h, 1).unwrap().label.unwrap().chars().count(), 80);
        act(&h, json!({"op": "rename", "port": 1, "value": "   "})).unwrap();
        assert_eq!(entry(&h, 1).unwrap().label, None);
        act(&h, json!({"op": "rename", "port": 1, "value": "x"})).unwrap();
        act(&h, json!({"op": "rename", "port": 1, "value": 42})).unwrap();
        assert_eq!(entry(&h, 1).unwrap().label, None);
    }

    #[test]
    fn forget_and_clear_stale() {
        let stale = Entry {
            listening: false,
            last_seen: 1,
            ..web(2)
        };
        let h = with_entries(None, vec![web(1), stale]);
        act(&h, json!({"op": "clear_stale"})).unwrap();
        assert!(entry(&h, 2).is_none());
        assert!(entry(&h, 1).is_some());
        act(&h, json!({"op": "forget", "port": 1})).unwrap();
        assert!(entry(&h, 1).is_none());
        act(&h, json!({"op": "forget", "port": 9})).unwrap();
    }

    #[test]
    fn invalid_actions_are_rejected() {
        let h = with_entries(None, vec![web(1)]);
        assert_eq!(act(&h, json!({"op": "pin"})), Err("missing port"));
        assert_eq!(
            act(&h, json!({"op": "pin", "port": 2})),
            Err("unknown port")
        );
        assert_eq!(
            act(&h, json!({"op": "explode", "port": 1})),
            Err("unknown op")
        );
    }

    #[test]
    fn recapture_queues_one_capture() {
        let h = with_entries(None, vec![web(1)]);
        assert_eq!(
            act(&h, json!({"op": "recapture", "port": 1})),
            Err("no browser available for thumbnails")
        );

        let h = with_entries(
            Some(PathBuf::from("/fake/browser")),
            vec![Entry {
                thumb_failed_at: 99,
                ..web(1)
            }],
        );
        act(&h, json!({"op": "recapture", "port": 1})).unwrap();
        act(&h, json!({"op": "recapture", "port": 1})).unwrap();
        assert_eq!(h.thumb_rx.try_recv(), Ok(1));
        assert!(h.thumb_rx.try_recv().is_err(), "already queued");
        assert_eq!(entry(&h, 1).unwrap().thumb_failed_at, 0);
    }

    // End-to-end through the real router on an ephemeral port.

    struct Client {
        port: u16,
    }

    struct Resp {
        status: u16,
        head: String,
        body: Vec<u8>,
    }

    impl Resp {
        fn header(&self, name: &str) -> Option<String> {
            self.head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
            })
        }

        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap()
        }
    }

    fn live(browser: Option<PathBuf>) -> (Harness, Client) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let h = harness(port, browser);
        let shared = h.shared.clone();
        thread::spawn(move || serve(server, shared));
        (h, Client { port })
    }

    impl Client {
        fn raw(
            &self,
            method: &str,
            path: &str,
            host: &str,
            extra: &[(&str, &str)],
            body: &str,
        ) -> Resp {
            let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut req =
                format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
            for (k, v) in extra {
                req.push_str(&format!("{k}: {v}\r\n"));
            }
            req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
            s.write_all(req.as_bytes()).unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).unwrap();
            let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let head = String::from_utf8_lossy(&buf[..split]).to_string();
            Resp {
                status: head[9..12].parse().unwrap(),
                head,
                body: buf[split + 4..].to_vec(),
            }
        }

        fn host(&self) -> String {
            format!("localhost:{}", self.port)
        }

        fn get(&self, path: &str) -> Resp {
            self.raw("GET", path, &self.host(), &[], "")
        }

        fn post(&self, path: &str, body: &str) -> Resp {
            self.raw(
                "POST",
                path,
                &self.host(),
                &[("Content-Type", "application/json")],
                body,
            )
        }
    }

    #[test]
    fn serves_the_static_ui() {
        let (_h, s) = live(None);
        for (path, ctype, content) in [
            ("/", "text/html; charset=utf-8", INDEX),
            ("/style.css", "text/css; charset=utf-8", STYLE),
            ("/app.js", "text/javascript; charset=utf-8", APP),
            ("/favicon.svg", "image/svg+xml", FAVICON),
        ] {
            let r = s.get(path);
            assert_eq!(r.status, 200, "{path}");
            assert_eq!(r.header("Content-Type").as_deref(), Some(ctype));
            assert_eq!(
                r.header("X-Content-Type-Options").as_deref(),
                Some("nosniff")
            );
            assert_eq!(r.body, content.as_bytes());
        }
        assert_eq!(s.get("/nope").status, 404);
        assert_eq!(s.post("/", "").status, 404);
    }

    #[test]
    fn rejects_foreign_hosts() {
        let (_h, s) = live(None);
        let evil = format!("evil.example:{}", s.port);
        assert_eq!(s.raw("GET", "/", &evil, &[], "").status, 403);
        assert_eq!(
            s.raw("GET", "/api/state", "localhost:1", &[], "").status,
            403
        );
        assert_eq!(
            s.raw("GET", "/", &format!("127.0.0.1:{}", s.port), &[], "")
                .status,
            200
        );
    }

    #[test]
    fn state_endpoint_supports_conditional_polling() {
        let (_h, s) = live(None);
        let r = s.get("/api/state");
        assert_eq!(r.status, 200);
        assert_eq!(r.header("Cache-Control").as_deref(), Some("no-store"));
        assert!(r.header("X-Scanned-At").is_some());
        let state = r.json();
        assert_eq!(state["thumbs"], false);
        assert_eq!(state["stale_minutes"], 10);
        assert_eq!(state["services"], json!([]));
        let v = state["version"].as_u64().unwrap();

        let r = s.get(&format!("/api/state?v={v}"));
        assert_eq!(r.status, 304);
        assert!(r.body.is_empty());
        assert_eq!(s.get(&format!("/api/state?v={}", v + 1)).status, 200);
    }

    #[test]
    fn state_poll_wakes_the_scanner_and_records_thumb_preference() {
        let (h, s) = live(None);
        s.get("/api/state");
        assert!(
            h.scan_rx.try_recv().is_err(),
            "already active: no extra scan"
        );

        s.get("/api/state?thumbs=0");
        assert!(!h.shared.store.lock().unwrap().thumbs_wanted);
        assert!(h.scan_rx.try_recv().is_err());

        s.get("/api/state?thumbs=1");
        assert!(h.shared.store.lock().unwrap().thumbs_wanted);
        assert!(matches!(
            h.scan_rx.try_recv(),
            Ok(ScanMsg::Refresh {
                force: false,
                done: None
            })
        ));

        if let Some(earlier) = Instant::now().checked_sub(Duration::from_secs(60)) {
            h.shared.store.lock().unwrap().last_client = earlier;
            s.get("/api/state");
            assert!(
                h.scan_rx.try_recv().is_ok(),
                "first poll after idle rescans"
            );
            assert!(h.shared.store.lock().unwrap().client_active());
        }
    }

    #[test]
    fn actions_over_http() {
        let (h, s) = live(None);
        {
            let mut store = h.shared.store.lock().unwrap();
            store.entries.insert(3000, web(3000));
            store.refresh_view();
        }
        let r = s.post(
            "/api/action",
            r#"{"op":"rename","port":3000,"value":"Docs"}"#,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["services"][0]["name"], "Docs");

        assert_eq!(s.post("/api/action", "{not json").status, 400);
        let r = s.post("/api/action", r#"{"op":"pin","port":1}"#);
        assert_eq!(
            (r.status, r.body.as_slice()),
            (400, b"unknown port".as_slice())
        );

        let plain = s.raw(
            "POST",
            "/api/action",
            &s.host(),
            &[("Content-Type", "text/plain")],
            r#"{"op":"forget","port":3000}"#,
        );
        assert_eq!(plain.status, 403);
        let cross = s.raw(
            "POST",
            "/api/action",
            &s.host(),
            &[
                ("Content-Type", "application/json"),
                ("Origin", "http://evil.example"),
            ],
            r#"{"op":"forget","port":3000}"#,
        );
        assert_eq!(cross.status, 403);
        assert!(h.shared.store.lock().unwrap().entries.contains_key(&3000));

        let origin = format!("http://{}", s.host());
        let same = s.raw(
            "POST",
            "/api/action",
            &s.host(),
            &[("Content-Type", "application/json"), ("Origin", &origin)],
            r#"{"op":"forget","port":3000}"#,
        );
        assert_eq!(same.status, 200);
        assert!(!h.shared.store.lock().unwrap().entries.contains_key(&3000));
    }

    #[test]
    fn refresh_waits_for_a_forced_scan() {
        let (h, s) = live(None);
        let (shared, scan_rx) = (h.shared.clone(), h.scan_rx);
        // A slow scanner that discovers a service: the response must include it.
        let scanner = thread::spawn(move || match scan_rx.recv().unwrap() {
            ScanMsg::Refresh { force, done } => {
                thread::sleep(Duration::from_millis(300));
                let mut store = shared.store.lock().unwrap();
                store.entries.insert(3000, web(3000));
                store.refresh_view();
                drop(store);
                done.unwrap().send(()).unwrap();
                force
            }
        });
        assert_eq!(
            s.raw("POST", "/api/refresh", &s.host(), &[], "").status,
            403
        );
        let r = s.post("/api/refresh", "");
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["services"][0]["port"], 3000);
        assert!(scanner.join().unwrap(), "refresh forces a full re-probe");
    }

    #[test]
    fn serves_cached_thumbnails_only() {
        let (h, s) = live(None);
        std::fs::write(Store::thumb_path(h.dir.path(), 3000), b"jpegbytes").unwrap();
        let r = s.get("/thumb/3000?v=1");
        assert_eq!(r.status, 200);
        assert_eq!(r.header("Content-Type").as_deref(), Some("image/jpeg"));
        assert_eq!(
            r.header("Cache-Control").as_deref(),
            Some("max-age=31536000, immutable")
        );
        assert_eq!(r.body, b"jpegbytes");
        assert_eq!(s.get("/thumb/3001").status, 404);
        assert_eq!(s.get("/thumb/abc").status, 404);
        assert_eq!(s.get("/thumb/../state.json").status, 404);
    }
}
