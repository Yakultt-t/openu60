//! MMS on the router: captures WAP Push notifications from the modem, downloads the
//! messages over the carrier's MMS APN, stores them for the app, and sends outgoing
//! messages through the MMSC. See docs/research/mms_feasibility_2026-11-05.md.

pub mod bearer;
pub mod handlers;
pub mod http;
pub mod pdu;
pub mod qmi;
pub mod store;
pub mod wap;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::push::PushService;
use store::{Record, Store};

const MAX_SEND_BYTES: usize = 1_200_000;
const PLACEHOLDER_HEX: &str = "006D0075006C00740069006D0065006400690061005F0069006E0066006F0072006D006100740069006F006E";
const SMS_DB: &str = "/etc_rw/ztembb/ztesms/sms_db/sms.db";

pub struct MmsService {
    pub store: Store,
    assembler: Mutex<wap::Assembler>,
    push: OnceLock<Arc<PushService>>,
    listener_state: Mutex<String>,
    /// One transfer at a time through the MMS PDN keeps the bearer logic simple.
    transfer: Mutex<()>,
}

/// Unix time. The firmware sets the system clock to the network's local time while
/// keeping TZ=UTC, so the raw clock runs ahead by the NITZ offset; take it back out.
fn now() -> u64 {
    let raw = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let synced = crate::ubus::uci_get("zte_nwinfo.sys_info.nitz_sync_flag").map(|v| v.trim() == "1").unwrap_or(false);
    if !synced {
        return raw;
    }
    let hours: f64 = crate::ubus::uci_get("zte_nwinfo.sys_info.nitz_timezone").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0.0);
    let offset = (hours * 3600.0) as i64;
    (raw as i64 - offset).max(0) as u64
}

impl MmsService {
    pub fn new() -> Self {
        Self {
            store: Store::open(),
            assembler: Mutex::new(wap::Assembler::new()),
            push: OnceLock::new(),
            listener_state: Mutex::new("stopped".into()),
            transfer: Mutex::new(()),
        }
    }

    pub fn start(self: &Arc<Self>, push: Arc<PushService>) {
        let _ = self.push.set(push);
        let me = Arc::clone(self);
        let me2 = Arc::clone(self);
        crate::system::spawn_named("mms-wms", move || {
            qmi::run(
                |pdu| me.on_pdu(pdu),
                |s| {
                    eprintln!("[mms] wms: {s}");
                    *me2.listener_state.lock().unwrap() = s;
                },
            );
        });
        // Anything left half-done by a previous run gets another try.
        let me = Arc::clone(self);
        crate::system::spawn_named("mms-resume", move || {
            std::thread::sleep(std::time::Duration::from_secs(20));
            for r in me.store.list() {
                if (r.status == "notified" || r.status == "downloading") && !r.content_location.is_empty() {
                    let _ = me.download(r.id);
                }
            }
        });
    }

    pub fn status(&self) -> Value {
        json!({
            "listener": *self.listener_state.lock().unwrap(),
            "bearer": bearer::status(),
            "count": self.store.list().len(),
        })
    }

    /// Every raw SMS PDU the modem stores passes through here; only WAP Push on the MMS
    /// port is of interest.
    fn on_pdu(&self, pdu: &[u8]) {
        let Some(d) = wap::parse_deliver(pdu) else {
            // Diagnostics only: shape of the PDU, never its content.
            let head: String = pdu.iter().take(4).map(|b| format!("{b:02x}")).collect();
            eprintln!("[mms] unparsed PDU ({} bytes, starts {head})", pdu.len());
            return;
        };
        if d.dest_port.is_some() || d.dcs & 0x0C == 0x04 {
            eprintln!(
                "[mms] PDU port {:?} dcs 0x{:02x} concat {:?} ({} bytes)",
                d.dest_port, d.dcs, d.concat.map(|(_, total, seq)| (seq, total)), d.data.len()
            );
        }
        if d.dest_port != Some(wap::WAP_PUSH_PORT) || d.dcs & 0x0C != 0x04 {
            return;
        }
        let Some(push) = self.assembler.lock().unwrap().push(&d) else { return };
        let Some(mms) = wap::unwrap_push(&push) else {
            eprintln!("[mms] WAP push from {} is not an MMS message", d.originator);
            return;
        };
        let Some(h) = pdu::decode_headers(mms) else {
            eprintln!("[mms] undecodable MMS push from {}", d.originator);
            return;
        };
        match h.message_type {
            pdu::MSG_NOTIFICATION_IND => self.on_notification(h),
            pdu::MSG_DELIVERY_IND => eprintln!("[mms] delivery report for {} ({})", h.message_id, h.to.join(",")),
            other => eprintln!("[mms] ignoring push message type 0x{other:02x}"),
        }
    }

    fn on_notification(&self, h: pdu::Headers) {
        if let Some(existing) = self.store.find_transaction(&h.transaction_id) {
            eprintln!("[mms] notification {} already known (status {})", h.transaction_id, existing.status);
            if existing.status == "failed" || existing.status == "notified" {
                let _ = self.download(existing.id);
            }
            return;
        }
        eprintln!("[mms] notification from {} size {} -> {}", h.from, h.message_size, h.content_location);
        let id = self.store.add(Record {
            id: 0,
            direction: "in".into(),
            number: h.from.clone(),
            subject: h.subject.clone(),
            text: String::new(),
            date: now(),
            message_id: String::new(),
            transaction_id: h.transaction_id.clone(),
            content_location: h.content_location.clone(),
            size: h.message_size,
            status: "notified".into(),
            error: String::new(),
            read: false,
            parts: Vec::new(),
        });
        let _ = self.download(id);
    }

    /// Fetches the message body for a notified record and stores its parts.
    pub fn download(&self, id: u64) -> Result<Record, String> {
        let Some(r) = self.store.get(id) else { return Err("no such message".into()) };
        if r.content_location.is_empty() {
            return Err("no content location".into());
        }
        self.store.update(id, |x| { x.status = "downloading".into(); x.error.clear(); });
        let result = (|| {
            let _guard = self.transfer.lock().unwrap();
            let profile = bearer::profile()?;
            let link = bearer::ensure_up(&profile)?;
            let reply = match http::get(&profile, &link, &r.content_location) {
                Ok(rep) => rep,
                Err(e) if Self::is_transient(&e) => {
                    eprintln!("[mms] download failed ({e}); re-dialling and retrying once");
                    let link = bearer::redial(&profile)?;
                    http::get(&profile, &link, &r.content_location)?
                }
                Err(e) => return Err(e),
            };
            if reply.status != 200 {
                return Err(format!("MMSC answered HTTP {}", reply.status));
            }
            let h = pdu::decode_headers(&reply.body).ok_or("undecodable m-retrieve.conf")?;
            if h.message_type != pdu::MSG_RETRIEVE_CONF {
                return Err(format!("unexpected message type 0x{:02x}", h.message_type));
            }
            let parts = pdu::decode_parts(&reply.body[h.body_at..]);
            let mut infos = Vec::new();
            let mut text = String::new();
            for (i, p) in parts.iter().enumerate() {
                if p.content_type.mime == "text/plain" && text.is_empty() {
                    text = pdu::decode_text_part(p);
                }
                if p.content_type.mime == "application/smil" {
                    continue; // layout only; the app lays parts out itself
                }
                let file = self.store.write_part(id, i, &p.content_type.mime, &p.data)?;
                infos.push(store::PartInfo { index: i, content_type: p.content_type.mime.clone(), name: p.name.clone(), size: p.data.len(), file });
            }
            let from = if h.from.is_empty() { r.number.clone() } else { h.from.clone() };
            self.store.update(id, |x| {
                x.status = "received".into();
                x.number = from;
                if !h.subject.is_empty() { x.subject = h.subject.clone(); }
                x.text = text.clone();
                x.message_id = h.message_id.clone();
                if let Some(d) = h.date { x.date = d; }
                x.size = reply.body.len() as u64;
                x.parts = infos;
            });
            // Tell the MMSC we have it (stops re-notification); best effort.
            match http::post(&profile, &link, &profile.mmsc, &pdu::encode_notify_resp(&h.transaction_id)) {
                Ok(rep) => eprintln!("[mms] notifyresp -> HTTP {}", rep.status),
                Err(e) => eprintln!("[mms] notifyresp failed: {e}"),
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                let rec = self.store.get(id).unwrap();
                eprintln!("[mms] received #{id} from {} ({} parts)", rec.number, rec.parts.len());
                self.drop_placeholders();
                self.notify(&rec);
                Ok(rec)
            }
            Err(e) => {
                eprintln!("[mms] download #{id} failed: {e}");
                self.store.update(id, |x| { x.status = "failed".into(); x.error = e.clone(); });
                Err(e)
            }
        }
    }

    /// The vendor daemon files every MMS notification as a text SMS reading
    /// "multimedia_information" from "99"; once the real message is in, those rows only
    /// confuse the list.
    fn drop_placeholders(&self) {
        let out = std::process::Command::new("sqlite3")
            .arg(SMS_DB)
            .arg(format!("select id from sms where Content like '{PLACEHOLDER_HEX}%'"))
            .output();
        let Ok(out) = out else { return };
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).map(|l| l.trim().to_string()).collect();
        if ids.is_empty() {
            return;
        }
        let list = ids.join(";") + ";";
        let _ = crate::ubus::call("zwrt_wms", "zwrt_wms_delete_sms", Some(&json!({"id": list}).to_string()));
        let _ = std::process::Command::new("sqlite3")
            .arg(SMS_DB)
            .arg(format!("delete from sms where id in ({})", ids.join(",")))
            .output();
        eprintln!("[mms] removed {} placeholder SMS row(s)", ids.len());
    }

    fn notify(&self, r: &Record) {
        if let Some(push) = self.push.get() {
            let body = if !r.text.is_empty() { r.text.clone() } else if !r.subject.is_empty() { r.subject.clone() } else { String::new() };
            push.notify_mms(&r.number, r.id, &body, r.parts.iter().filter(|p| p.content_type.starts_with("image/")).count());
        }
    }

    /// Errors worth one more attempt on a freshly dialled bearer: the proxy not answering,
    /// or the MMSC refusing the originator (seen after the PDN had been idle for a while).
    fn is_transient(e: &str) -> bool {
        e.contains("no HTTP reply") || e.contains("timed out") || e.contains("0xe1") || e.contains("0xe0") || e.contains("HTTP 5")
    }

    /// POSTs an m-send.req to the MMSC; on a transient failure re-dials the PDN and tries once more.
    fn post_send_req(&self, body: &[u8]) -> Result<String, String> {
        let _guard = self.transfer.lock().unwrap();
        let profile = bearer::profile()?;
        let attempt = |link: &bearer::Link| -> Result<String, String> {
            let reply = http::post(&profile, link, &profile.mmsc, body)?;
            if reply.status != 200 {
                return Err(format!("MMSC answered HTTP {}", reply.status));
            }
            let h = pdu::decode_headers(&reply.body).ok_or("undecodable m-send.conf")?;
            match h.response_status {
                Some(0x80) => Ok(h.message_id),
                Some(code) => Err(format!("MMSC rejected: 0x{code:02x} {}", h.response_text)),
                None => Err("m-send.conf without status".into()),
            }
        };
        let link = bearer::ensure_up(&profile)?;
        match attempt(&link) {
            Ok(id) => Ok(id),
            Err(e) if Self::is_transient(&e) => {
                eprintln!("[mms] send failed ({e}); re-dialling and retrying once");
                let link = bearer::redial(&profile)?;
                attempt(&link)
            }
            Err(e) => Err(e),
        }
    }

    /// Sends a stored outgoing message again (after a send failure).
    pub fn resend(&self, id: u64) -> Result<Record, String> {
        let Some(r) = self.store.get(id) else { return Err("no such message".into()) };
        if r.direction != "out" {
            return Err("not an outgoing message".into());
        }
        self.store.update(id, |x| { x.status = "sending".into(); x.error.clear(); });
        let mut parts: Vec<(String, String, Vec<u8>)> = Vec::new();
        for p in &r.parts {
            let data = std::fs::read(std::path::PathBuf::from(store::DIR).join(&p.file)).map_err(|e| format!("read part: {e}"))?;
            parts.push((p.content_type.clone(), p.name.clone(), data));
        }
        let txid = format!("openu60-{}-{}", id, now());
        let mut out_parts: Vec<pdu::OutPart> = Vec::new();
        if !r.text.is_empty() {
            out_parts.push(pdu::OutPart { mime: "text/plain", name: "text.txt", data: r.text.as_bytes() });
        }
        for (mime, name, data) in &parts {
            out_parts.push(pdu::OutPart { mime, name, data });
        }
        let body = pdu::encode_send_req(&txid, &[r.number.clone()], &r.subject, &out_parts);
        match self.post_send_req(&body) {
            Ok(message_id) => {
                self.store.update(id, |x| { x.status = "sent".into(); x.message_id = message_id.clone(); x.transaction_id = txid.clone(); x.date = now(); });
                eprintln!("[mms] resent #{id}: {message_id}");
                Ok(self.store.get(id).unwrap())
            }
            Err(e) => {
                self.store.update(id, |x| { x.status = "send_failed".into(); x.error = e.clone(); });
                eprintln!("[mms] resend #{id} failed: {e}");
                Err(e)
            }
        }
    }

    /// Sends a message; blocks until the MMSC answers.
    pub fn send(&self, number: &str, subject: &str, text: &str, parts: Vec<(String, String, Vec<u8>)>) -> Result<Record, String> {
        let number = number.trim();
        if number.is_empty() {
            return Err("missing number".into());
        }
        let total: usize = parts.iter().map(|p| p.2.len()).sum::<usize>() + text.len();
        if total > MAX_SEND_BYTES {
            return Err(format!("message too large ({} bytes)", total));
        }
        if parts.is_empty() && text.is_empty() {
            return Err("nothing to send".into());
        }
        let id = self.store.add(Record {
            id: 0, direction: "out".into(), number: number.to_string(), subject: subject.to_string(), text: text.to_string(),
            date: now(), message_id: String::new(), transaction_id: String::new(), content_location: String::new(),
            size: total as u64, status: "sending".into(), error: String::new(), read: true, parts: Vec::new(),
        });
        let mut infos = Vec::new();
        let mut index = 0;
        for (mime, name, data) in &parts {
            if let Ok(file) = self.store.write_part(id, index, mime, data) {
                infos.push(store::PartInfo { index, content_type: mime.clone(), name: name.clone(), size: data.len(), file });
            }
            index += 1;
        }
        self.store.update(id, |x| x.parts = infos);
        let txid = format!("openu60-{}-{}", id, now());
        let mut out_parts: Vec<pdu::OutPart> = Vec::new();
        if !text.is_empty() {
            out_parts.push(pdu::OutPart { mime: "text/plain", name: "text.txt", data: text.as_bytes() });
        }
        for (mime, name, data) in &parts {
            out_parts.push(pdu::OutPart { mime, name, data });
        }
        let body = pdu::encode_send_req(&txid, &[number.to_string()], subject, &out_parts);
        let result = self.post_send_req(&body);
        match result {
            Ok(message_id) => {
                self.store.update(id, |x| { x.status = "sent".into(); x.message_id = message_id.clone(); x.transaction_id = txid.clone(); });
                eprintln!("[mms] sent #{id} to {number}: {message_id}");
            }
            Err(e) => {
                self.store.update(id, |x| { x.status = "send_failed".into(); x.error = e.clone(); });
                eprintln!("[mms] send #{id} failed: {e}");
                return Err(e);
            }
        }
        Ok(self.store.get(id).unwrap())
    }
}
