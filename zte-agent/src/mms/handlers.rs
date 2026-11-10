//! /api/mms/* — the app's view of the MMS store.

use base64::Engine;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, StatusCode};

use crate::handlers::AppState;

fn record_json(r: &super::store::Record) -> Value {
    json!({
        "id": r.id, "direction": r.direction, "number": r.number, "subject": r.subject, "text": r.text,
        "date": r.date, "message_id": r.message_id, "status": r.status, "error": r.error, "read": r.read, "size": r.size,
        "parts": r.parts.iter().map(|p| json!({"index": p.index, "content_type": p.content_type, "name": p.name, "size": p.size})).collect::<Vec<_>>(),
    })
}

/// Dispatches the MMS routes; `None` for anything else.
pub fn route(method: &Method, path: &str, state: &AppState, body: &[u8]) -> Option<(u16, Value)> {
    let rest = path.strip_prefix("/api/mms")?;
    let mms = &state.mms;
    Some(match (method, rest) {
        (&Method::Get, "/list") | (&Method::Get, "") => {
            let list: Vec<Value> = mms.store.list().iter().map(record_json).collect();
            (200, json!({"ok": true, "data": {"messages": list}}))
        }
        (&Method::Get, "/status") => (200, json!({"ok": true, "data": mms.status()})),
        (&Method::Post, "/send") => {
            let parsed: Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return Some((400, json!({"ok": false, "error": "invalid JSON"}))),
            };
            let number = parsed["number"].as_str().unwrap_or("");
            let subject = parsed["subject"].as_str().unwrap_or("");
            let text = parsed["text"].as_str().unwrap_or("");
            let mut parts = Vec::new();
            if let Some(arr) = parsed["parts"].as_array() {
                for (i, p) in arr.iter().enumerate() {
                    let mime = p["content_type"].as_str().unwrap_or("application/octet-stream").to_ascii_lowercase();
                    let name = p["name"].as_str().map(|s| s.to_string()).unwrap_or_else(|| format!("part{i}"));
                    let data = match p["data"].as_str().map(|s| base64::engine::general_purpose::STANDARD.decode(s)) {
                        Some(Ok(d)) => d,
                        _ => return Some((400, json!({"ok": false, "error": format!("part {i}: data must be base64")}))),
                    };
                    parts.push((mime, name, data));
                }
            }
            match mms.send(number, subject, text, parts) {
                Ok(r) => (200, json!({"ok": true, "data": record_json(&r)})),
                Err(e) => (502, json!({"ok": false, "error": e})),
            }
        }
        (&Method::Post, "/mark-read") => {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let mut n = 0;
            if let Some(ids) = parsed["ids"].as_array() {
                for id in ids.iter().filter_map(|v| v.as_u64()) {
                    if mms.store.update(id, |r| r.read = true) { n += 1; }
                }
            }
            (200, json!({"ok": true, "data": {"updated": n}}))
        }
        (m, rest) => {
            // /{id}, /{id}/retry
            let mut segs = rest.trim_start_matches('/').split('/');
            let Some(id) = segs.next().and_then(|s| s.parse::<u64>().ok()) else {
                return Some((404, json!({"ok": false, "error": "not found"})));
            };
            match (m, segs.next()) {
                (&Method::Get, None) => match mms.store.get(id) {
                    Some(r) => (200, json!({"ok": true, "data": record_json(&r)})),
                    None => (404, json!({"ok": false, "error": "no such message"})),
                },
                (&Method::Delete, None) => {
                    if mms.store.delete(id) { (200, json!({"ok": true})) } else { (404, json!({"ok": false, "error": "no such message"})) }
                }
                (&Method::Post, Some("retry")) => {
                    let outgoing = mms.store.get(id).map(|r| r.direction == "out").unwrap_or(false);
                    let result = if outgoing { mms.resend(id) } else { mms.download(id) };
                    match result {
                        Ok(r) => (200, json!({"ok": true, "data": record_json(&r)})),
                        Err(e) => (502, json!({"ok": false, "error": e})),
                    }
                }
                _ => (404, json!({"ok": false, "error": "not found"})),
            }
        }
    })
}

/// GET /api/mms/{id}/part/{n}: the raw part bytes with their content type.
pub fn serve_part(request: Request, state: &AppState, path: &str) {
    let mut segs = path.trim_start_matches("/api/mms/").split('/');
    let id = segs.next().and_then(|s| s.parse::<u64>().ok());
    let n = match (segs.next(), segs.next()) {
        (Some("part"), Some(n)) => n.parse::<usize>().ok(),
        _ => None,
    };
    let found = id.zip(n).and_then(|(id, n)| state.mms.store.part_path(id, n));
    let Some((file, mime)) = found else {
        let _ = request.respond(Response::from_string("{\"ok\":false,\"error\":\"no such part\"}").with_status_code(404));
        return;
    };
    match std::fs::read(&file) {
        Ok(data) => {
            let len = data.len();
            let response = Response::new(
                StatusCode(200),
                vec![
                    Header::from_bytes("Content-Type", mime.as_bytes()).unwrap(),
                    Header::from_bytes("Cache-Control", "private, max-age=86400").unwrap(),
                ],
                std::io::Cursor::new(data),
                Some(len),
                None,
            );
            let _ = request.respond(response);
        }
        Err(_) => {
            let _ = request.respond(Response::from_string("{\"ok\":false,\"error\":\"part missing\"}").with_status_code(404));
        }
    }
}

pub fn is_part_path(path: &str) -> bool {
    path.starts_with("/api/mms/") && path.contains("/part/")
}
