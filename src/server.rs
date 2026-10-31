//! Small HTTP server. Read-only GET endpoints plus one token-protected
//! POST endpoint for control actions (only live with `--allow-control`).
//!
//!   GET  /               dashboard
//!   GET  /api/state      latest sample (radios, stations, hw, plan, advice)
//!   GET  /api/history    time series
//!   GET  /api/events     event log, `?since=ID` for incremental reads
//!   POST /api/control    JSON action, header `X-Token: <token>`
use crate::collector::Collector;
use crate::control::{token_ok, Action};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Response, Server};

const INDEX: &str = include_str!("../static/index.html");
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'";
const MAX_BODY: u64 = 4096;

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn respond(req: tiny_http::Request, code: u16, body: Vec<u8>, ctype: &str) {
    let resp = Response::from_data(body)
        .with_status_code(code)
        .with_header(header("Content-Type", ctype))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("Content-Security-Policy", CSP))
        .with_header(header("X-Content-Type-Options", "nosniff"));
    let _ = req.respond(resp);
}

fn json(req: tiny_http::Request, code: u16, v: serde_json::Value) {
    respond(req, code, v.to_string().into_bytes(), "application/json");
}

fn query_u64(url: &str, key: &str) -> u64 {
    url.split_once('?')
        .map(|x| x.1)
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0)
}

fn handle_control(mut req: tiny_http::Request, col: &Collector) {
    let ctl = &col.ctl;
    if !ctl.cfg.enabled {
        return json(req, 403, serde_json::json!({"ok": false, "error": "control is disabled (start with --allow-control)"}));
    }
    let given = req.headers().iter().find(|h| h.field.equiv("X-Token")).map(|h| h.value.as_str().to_string()).unwrap_or_default();
    if !token_ok(&ctl.cfg.token, &given) {
        std::thread::sleep(Duration::from_millis(400)); // slow down guessing
        return json(req, 401, serde_json::json!({"ok": false, "error": "bad or missing X-Token"}));
    }
    let mut body = String::new();
    if req.as_reader().take(MAX_BODY + 1).read_to_string(&mut body).is_err() || body.len() as u64 > MAX_BODY {
        return json(req, 400, serde_json::json!({"ok": false, "error": "bad or oversized body"}));
    }
    let action: Action = match serde_json::from_str(&body) {
        Ok(a) => a,
        Err(e) => return json(req, 400, serde_json::json!({"ok": false, "error": format!("bad action: {e}")})),
    };
    let who = format!("ui:{}", action.name());
    match ctl.execute(&who, &action) {
        Ok(out) => json(req, 200, serde_json::json!({"ok": true, "output": out})),
        Err(e) => json(req, 200, serde_json::json!({"ok": false, "error": e})),
    }
}

pub fn serve(col: Arc<Collector>, log: Arc<crate::events::EventLog>, bind: &str, port: u16, demo: bool) -> Result<(), String> {
    let server = Server::http((bind, port)).map_err(|e| format!("cannot listen on {bind}:{port}: {e}"))?;
    let host = if bind.contains(':') { format!("[{bind}]") } else { bind.to_string() };
    eprintln!("mt7996app {} listening on http://{host}:{port}/{}", env!("CARGO_PKG_VERSION"), if demo { "  (demo data)" } else { "" });
    if col.ctl.cfg.enabled {
        eprintln!(
            "control actions ENABLED{}{}{}",
            if col.ctl.cfg.dry_run { " (dry-run)" } else { "" },
            if col.ctl.cfg.airtime_auto { ", airtime auto" } else { "" },
            if col.ctl.cfg.steer_auto { ", steering auto" } else { "" }
        );
    }
    for req in server.incoming_requests() {
        let col = col.clone();
        let log = log.clone();
        std::thread::spawn(move || {
            let url = req.url().to_string();
            let path = url.split('?').next().unwrap_or("/").to_string();
            match (req.method(), path.as_str()) {
                (tiny_http::Method::Post, "/api/control") => handle_control(req, &col),
                (tiny_http::Method::Get, "/" | "/index.html") => respond(req, 200, INDEX.as_bytes().to_vec(), "text/html; charset=utf-8"),
                (tiny_http::Method::Get, "/api/state") => {
                    let body = col.state_json().as_bytes().to_vec();
                    respond(req, 200, body, "application/json")
                }
                (tiny_http::Method::Get, "/api/history") => respond(req, 200, col.history_json().into_bytes(), "application/json"),
                (tiny_http::Method::Get, "/api/events") => {
                    respond(req, 200, log.json(query_u64(&url, "since")).into_bytes(), "application/json")
                }
                (_, "/" | "/index.html" | "/api/state" | "/api/history" | "/api/events" | "/api/control") => {
                    respond(req, 405, b"method not allowed\n".to_vec(), "text/plain")
                }
                _ => respond(req, 404, b"not found\n".to_vec(), "text/plain"),
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::query_u64;

    #[test]
    fn since_query() {
        assert_eq!(query_u64("/api/events?since=42", "since"), 42);
        assert_eq!(query_u64("/api/events?x=1&since=7", "since"), 7);
        assert_eq!(query_u64("/api/events", "since"), 0);
        assert_eq!(query_u64("/api/events?since=abc", "since"), 0);
    }
}
