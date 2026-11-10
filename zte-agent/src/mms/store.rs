//! On-box MMS storage: a JSON index plus one file per part under /data/openu60/mms.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const DIR: &str = "/data/openu60/mms";
const MAX_RECORDS: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartInfo {
    pub index: usize,
    pub content_type: String,
    pub name: String,
    pub size: usize,
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: u64,
    /// "in" or "out"
    pub direction: String,
    pub number: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub text: String,
    /// Unix seconds.
    pub date: u64,
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub transaction_id: String,
    #[serde(default)]
    pub content_location: String,
    #[serde(default)]
    pub size: u64,
    /// notified | downloading | received | failed | sending | sent | send_failed
    pub status: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub parts: Vec<PartInfo>,
}

pub struct Store {
    records: Mutex<Vec<Record>>,
}

impl Store {
    pub fn open() -> Self {
        let _ = std::fs::create_dir_all(DIR);
        let records = std::fs::read_to_string(Self::index_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { records: Mutex::new(records) }
    }

    fn index_path() -> PathBuf {
        PathBuf::from(DIR).join("index.json")
    }

    fn save(records: &[Record]) {
        if let Ok(s) = serde_json::to_string(records) {
            let tmp = PathBuf::from(DIR).join("index.json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, Self::index_path());
            }
        }
    }

    pub fn list(&self) -> Vec<Record> {
        self.records.lock().unwrap().clone()
    }

    pub fn get(&self, id: u64) -> Option<Record> {
        self.records.lock().unwrap().iter().find(|r| r.id == id).cloned()
    }

    /// Adds a record (its id is assigned here) and returns the id.
    pub fn add(&self, mut r: Record) -> u64 {
        let mut g = self.records.lock().unwrap();
        let id = g.iter().map(|x| x.id).max().unwrap_or(0) + 1;
        r.id = id;
        g.push(r);
        while g.len() > MAX_RECORDS {
            let old = g.remove(0);
            Self::remove_files(&old);
        }
        Self::save(&g);
        id
    }

    pub fn update(&self, id: u64, f: impl FnOnce(&mut Record)) -> bool {
        let mut g = self.records.lock().unwrap();
        let Some(r) = g.iter_mut().find(|r| r.id == id) else { return false };
        f(r);
        Self::save(&g);
        true
    }

    pub fn delete(&self, id: u64) -> bool {
        let mut g = self.records.lock().unwrap();
        let Some(pos) = g.iter().position(|r| r.id == id) else { return false };
        let old = g.remove(pos);
        Self::remove_files(&old);
        Self::save(&g);
        true
    }

    /// Already handled this transaction (MMSCs re-send notifications until acknowledged)?
    pub fn find_transaction(&self, txid: &str) -> Option<Record> {
        if txid.is_empty() {
            return None;
        }
        self.records.lock().unwrap().iter().find(|r| r.transaction_id == txid).cloned()
    }

    pub fn write_part(&self, id: u64, index: usize, mime: &str, data: &[u8]) -> Result<String, String> {
        let ext = match mime {
            "image/jpeg" | "image/jpg" => "jpg",
            "image/png" => "png",
            "image/gif" => "gif",
            "text/plain" => "txt",
            "application/smil" => "smil",
            "audio/mp4" | "audio/m4a" | "audio/aac" => "m4a",
            "audio/amr" => "amr",
            "video/3gpp" => "3gp",
            "video/mp4" => "mp4",
            _ => "bin",
        };
        let file = format!("{id}-{index}.{ext}");
        std::fs::write(PathBuf::from(DIR).join(&file), data).map_err(|e| format!("write part: {e}"))?;
        Ok(file)
    }

    pub fn part_path(&self, id: u64, index: usize) -> Option<(PathBuf, String)> {
        let r = self.get(id)?;
        let p = r.parts.into_iter().find(|p| p.index == index)?;
        Some((PathBuf::from(DIR).join(p.file), p.content_type))
    }

    fn remove_files(r: &Record) {
        for p in &r.parts {
            let _ = std::fs::remove_file(PathBuf::from(DIR).join(&p.file));
        }
    }
}
