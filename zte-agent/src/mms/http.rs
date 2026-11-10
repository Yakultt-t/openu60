//! HTTP through the MMS PDN: the firmware ships curl, which binds to an interface and
//! talks to the carrier proxy without any routing changes on the box.

use std::process::Command;

use super::bearer::{Link, Profile};

pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

fn run(p: &Profile, link: &Link, url: &str, post: Option<&[u8]>) -> Result<Reply, String> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let out_path = format!("/tmp/openu60-mms-{stamp}.out");
    let in_path = format!("/tmp/openu60-mms-{stamp}.in");
    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-S", "-m", "90", "--interface", &link.device, "-o", &out_path, "-w", "%{http_code}",
              "-H", "Accept: application/vnd.wap.mms-message, */*", "-H", "User-Agent: OpenU60/1.0",
              "-H", "Accept-Language: en", "--http1.1"]);
    if !p.proxy.is_empty() {
        cmd.args(["-x", &format!("http://{}", p.proxy)]);
    }
    if let Some(body) = post {
        std::fs::write(&in_path, body).map_err(|e| format!("write body: {e}"))?;
        cmd.args(["-X", "POST", "-H", "Content-Type: application/vnd.wap.mms-message", "--data-binary", &format!("@{in_path}")]);
    }
    cmd.arg(url);
    let result = cmd.output();
    let _ = std::fs::remove_file(&in_path);
    let output = result.map_err(|e| format!("curl: {e}"))?;
    let code = String::from_utf8_lossy(&output.stdout).trim().parse::<u16>().unwrap_or(0);
    let body = std::fs::read(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    if code == 0 {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(format!("no HTTP reply (exit {:?}; stderr: {err}; stdout: {out})", output.status.code()));
    }
    Ok(Reply { status: code, body })
}

pub fn get(p: &Profile, link: &Link, url: &str) -> Result<Reply, String> {
    run(p, link, url, None)
}

pub fn post(p: &Profile, link: &Link, url: &str, body: &[u8]) -> Result<Reply, String> {
    run(p, link, url, Some(body))
}
