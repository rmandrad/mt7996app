//! Tiny read-only HTTP server: `/`, `/api/state`, `/api/history`.
use crate::collector::Collector;
use std::sync::Arc;
use tiny_http::{Header, Response, Server};

const INDEX: &str = include_str!("../static/index.html");
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'";

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

pub fn serve(col: Arc<Collector>, bind: &str, port: u16, demo: bool) -> Result<(), String> {
    let server = Server::http((bind, port)).map_err(|e| format!("cannot listen on {bind}:{port}: {e}"))?;
    let host = if bind.contains(':') { format!("[{bind}]") } else { bind.to_string() };
    eprintln!("mt7996app {} listening on http://{host}:{port}/{}", env!("CARGO_PKG_VERSION"), if demo { "  (demo data)" } else { "" });
    for req in server.incoming_requests() {
        let col = col.clone();
        std::thread::spawn(move || {
            if *req.method() != tiny_http::Method::Get {
                return respond(req, 405, b"method not allowed\n".to_vec(), "text/plain");
            }
            let path = req.url().split('?').next().unwrap_or("/").to_string();
            match path.as_str() {
                "/" | "/index.html" => respond(req, 200, INDEX.as_bytes().to_vec(), "text/html; charset=utf-8"),
                "/api/state" => {
                    let body = col.state_json().as_bytes().to_vec();
                    respond(req, 200, body, "application/json")
                }
                "/api/history" => respond(req, 200, col.history_json().into_bytes(), "application/json"),
                _ => respond(req, 404, b"not found\n".to_vec(), "text/plain"),
            }
        });
    }
    Ok(())
}
