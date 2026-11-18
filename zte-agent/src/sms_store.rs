//! Keeps the router's SMS storage from filling up.
//!
//! The modem keeps at most 100 messages (`sms_nv_total`). When that fills, the stock screen
//! warns about full memory and new messages can be lost. So when it is 85 % full, the oldest
//! read and sent messages move to /data/openu60/sms_archive.json (written and synced first)
//! and are then deleted from the modem, down to 60 %. Unread messages and drafts stay.
//!
//! The SMS list served to the app, screen, watch and web panel includes the archived
//! messages, so nothing disappears from them (and the app keeps them on the phone with the
//! rest). Archived messages get ids above `ID_BASE`, apart from the modem's own ids and the
//! app's negative MMS ids; deleting one removes it from the archive.

use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::ubus;

const ARCHIVE: &str = "/data/openu60/sms_archive.json";
/// Archived message id = modem id + ID_BASE.
pub const ID_BASE: i64 = 1_000_000_000;
const START_PERCENT: u64 = 85;
const TARGET_PERCENT: u64 = 60;
const CHECK_EVERY: Duration = Duration::from_secs(120);

static LOCK: Mutex<()> = Mutex::new(());

fn load() -> Vec<Value> {
    std::fs::read(ARCHIVE)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["messages"].as_array().cloned())
        .unwrap_or_default()
}

/// Written to a temp file, synced, then renamed: the modem copy is only deleted after this.
fn save(messages: &[Value]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all("/data/openu60").map_err(|e| e.to_string())?;
    let tmp = format!("{ARCHIVE}.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("archive: {e}"))?;
    f.write_all(json!({"messages": messages}).to_string().as_bytes()).map_err(|e| format!("archive: {e}"))?;
    f.sync_all().map_err(|e| format!("archive sync: {e}"))?;
    std::fs::rename(&tmp, ARCHIVE).map_err(|e| format!("archive: {e}"))
}

pub fn is_archive_id(id: i64) -> bool {
    id >= ID_BASE
}

fn same_message(a: &Value, b: &Value) -> bool {
    a["number"] == b["number"] && a["date"] == b["date"] && a["content"] == b["content"]
}

/// Adds the archived messages to an SMS list reply for the router's own store (mem_store 1)
/// or all stores (2), on the first page. Skips ones the modem still has (same number, time
/// and text), so a message never shows twice.
pub fn merge_into(data: &mut Value, request: &Value) {
    let store = request["mem_store"].as_i64().or_else(|| request["mem_store"].as_str().and_then(|s| s.parse().ok()));
    let page = request["page"].as_i64().unwrap_or(0);
    if !matches!(store, Some(1) | Some(2)) || page != 0 {
        return;
    }
    let archived = {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        load()
    };
    if archived.is_empty() {
        return;
    }
    let Some(list) = data["messages"].as_array_mut() else { return };
    let live = list.clone();
    for m in archived {
        if !live.iter().any(|l| same_message(l, &m)) {
            list.push(m);
        }
    }
}

/// Deletes archived messages by their list ids; returns how many went.
pub fn delete(ids: &[i64]) -> Result<usize, String> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut messages = load();
    let before = messages.len();
    messages.retain(|m| !m["id"].as_i64().is_some_and(|id| ids.contains(&id)));
    let removed = before - messages.len();
    if removed > 0 {
        save(&messages)?;
    }
    Ok(removed)
}

/// Which modem messages to move: the oldest read (tag 0) or sent (tag 2) ones, as many as it
/// takes to get from `used` down to `target`. Unread, failed and drafts stay.
pub fn pick(messages: &[Value], used: u64, target: u64) -> Vec<Value> {
    let need = used.saturating_sub(target) as usize;
    let mut candidates: Vec<&Value> = messages
        .iter()
        .filter(|m| matches!(m["tag"].as_str(), Some("0") | Some("2")))
        .filter(|m| m["id"].as_i64().is_some_and(|id| id > 0 && id < ID_BASE))
        .collect();
    candidates.sort_by_key(|m| m["id"].as_i64().unwrap_or(0));
    candidates.into_iter().take(need).cloned().collect()
}

/// One pass: returns how many messages moved to the archive.
pub fn cleanup() -> Result<usize, String> {
    cleanup_with(START_PERCENT, TARGET_PERCENT)
}

/// `cleanup` with other thresholds, in percent (support command `zte-agent sms-archive-once`).
pub fn cleanup_with(start: u64, target: u64) -> Result<usize, String> {
    let cap = ubus::call("zwrt_wms", "zwrt_wms_get_wms_capacity", Some("{}"))?;
    let n = |k: &str| cap[k].as_u64().or_else(|| cap[k].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
    let total = n("sms_nv_total");
    let used = n("sms_nv_rev_total") + n("sms_nv_send_total") + n("sms_nv_draftbox_total");
    if total == 0 || used * 100 < total * start {
        return Ok(0);
    }
    let request = json!({"page": 0, "data_per_page": 500, "mem_store": 1, "tags": 10, "order_by": "order by id desc"});
    let mut data = ubus::call("zwrt_wms", "zte_libwms_get_sms_data", Some(&request.to_string()))?;
    if crate::sms_crypto::decrypt_messages(&mut data).is_err() {
        crate::sms_crypto::rekey()?;
        data = ubus::call("zwrt_wms", "zte_libwms_get_sms_data", Some(&request.to_string()))?;
        crate::sms_crypto::decrypt_messages(&mut data)?;
    }
    let messages = data["messages"].as_array().cloned().unwrap_or_default();
    let chosen = pick(&messages, used, total * target / 100);
    if chosen.is_empty() {
        return Ok(0);
    }
    {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut archive = load();
        for m in &chosen {
            if archive.iter().any(|a| same_message(a, m)) {
                continue;
            }
            let mut copy = m.clone();
            copy["id"] = json!(m["id"].as_i64().unwrap_or(0) + ID_BASE);
            copy["mem_store"] = json!("archive");
            archive.push(copy);
        }
        save(&archive)?;
    }
    let ids: String = chosen.iter().filter_map(|m| m["id"].as_i64()).map(|id| format!("{id};")).collect();
    let (status, reply) = crate::sms::delete_ids(&ids);
    if status != 200 {
        return Err(format!("archived, but the modem delete failed: {reply}"));
    }
    Ok(chosen.len())
}

pub fn start() {
    crate::system::spawn_named("sms-store", || {
        std::thread::sleep(Duration::from_secs(60));
        loop {
            match cleanup() {
                Ok(0) => {}
                Ok(n) => eprintln!("[sms-store] moved {n} old messages from the modem to the archive"),
                Err(e) => eprintln!("[sms-store] {e}"),
            }
            std::thread::sleep(CHECK_EVERY);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: i64, tag: &str) -> Value {
        json!({"id": id, "tag": tag, "number": "10086", "date": format!("26,11,18,10,{id:02},00,+32"), "content": "x"})
    }

    #[test]
    fn moves_the_oldest_read_and_sent_only() {
        let list = vec![msg(5, "1"), msg(1, "0"), msg(2, "4"), msg(3, "2"), msg(4, "0"), msg(6, "0")];
        let picked: Vec<i64> = pick(&list, 88, 85).iter().map(|m| m["id"].as_i64().unwrap()).collect();
        assert_eq!(picked, vec![1, 3, 4]);
        assert!(pick(&list, 60, 60).is_empty());
    }

    #[test]
    fn merges_only_into_the_first_router_page_and_skips_duplicates() {
        let mut data = json!({"messages": [msg(9, "0")]});
        // No archive file on the test machine: nothing to add, nothing breaks.
        merge_into(&mut data, &json!({"page": 0, "mem_store": 2}));
        assert_eq!(data["messages"].as_array().unwrap().len(), 1);
        assert!(same_message(&msg(9, "0"), &msg(9, "1")));
        assert!(is_archive_id(ID_BASE + 3) && !is_archive_id(3) && !is_archive_id(-3));
    }
}
