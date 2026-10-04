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
    for req in server.incoming_requests() {
        let shared = shared.clone();
        thread::spawn(move || handle(req, &shared));
    }
    Ok(())
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
    let allowed = [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
    ];
    req_header(req, "Host").is_some_and(|h| allowed.iter().any(|a| a == h))
}

/// Mutations must come from our own page: JSON content type (forces a CORS preflight
/// for cross-origin pages) and, when present, a matching Origin.
fn mutation_ok(req: &Request) -> bool {
    let json = req_header(req, "Content-Type").is_some_and(|c| c.starts_with("application/json"));
    let origin_ok = match (req_header(req, "Origin"), req_header(req, "Host")) {
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
