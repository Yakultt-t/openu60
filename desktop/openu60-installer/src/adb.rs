//! Finding (or fetching) Google's adb and running it. Binary output is read straight from
//! the child's stdout, never through a shell, so it stays intact on Windows too.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ui;

#[derive(Clone)]
pub struct Adb {
    pub path: PathBuf,
}

const EXE: &str = if cfg!(windows) { "adb.exe" } else { "adb" };

fn data_dir() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("OpenU60")
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support/OpenU60")
    } else {
        home().join(".local/share/openu60")
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn candidates() -> Vec<PathBuf> {
    let mut list = Vec::new();
    if let Some(p) = std::env::var_os("ADB") {
        list.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            list.push(dir.join(EXE));
            list.push(dir.join("platform-tools").join(EXE));
        }
    }
    list.push(data_dir().join("platform-tools").join(EXE));
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            list.push(dir.join(EXE));
        }
    }
    if cfg!(target_os = "macos") {
        list.push(PathBuf::from("/opt/homebrew/bin/adb"));
        list.push(PathBuf::from("/usr/local/bin/adb"));
        list.push(home().join("Library/Android/sdk/platform-tools/adb"));
    }
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            list.push(PathBuf::from(local).join("Android/Sdk/platform-tools/adb.exe"));
        }
    }
    list
}

pub fn find() -> Option<Adb> {
    candidates().into_iter().find(|p| p.is_file()).map(|path| Adb { path })
}

/// Downloads a zip from Google and unpacks it into the per-user data folder.
pub fn fetch_zip(url: &str, what: &str) -> Result<PathBuf, String> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建 {}：{e}", dir.display()))?;
    ui::info(&format!("正在从 Google 下载{what}（{url}，约 9 MB）……"));
    let mut resp = ureq::get(url).call().map_err(|e| format!("下载失败：{e}"))?;
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(200 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| format!("下载失败：{e}"))?;
    let zip = dir.join("download.zip");
    std::fs::write(&zip, &bytes).map_err(|e| format!("保存失败：{e}"))?;
    // Windows 10 and later ship bsdtar, which unpacks zip files; macOS and Linux have unzip.
    let status = if cfg!(windows) {
        Command::new("tar").arg("-xf").arg(&zip).arg("-C").arg(&dir).status()
    } else {
        Command::new("unzip").arg("-q").arg("-o").arg(&zip).arg("-d").arg(&dir).status()
    };
    let _ = std::fs::remove_file(&zip);
    match status {
        Ok(s) if s.success() => Ok(dir),
        _ => Err(format!("解压{what}失败")),
    }
}

/// Downloads Google's platform-tools (adb).
pub fn download() -> Result<Adb, String> {
    let os = if cfg!(windows) { "windows" } else if cfg!(target_os = "macos") { "darwin" } else { "linux" };
    let dir = fetch_zip(&format!("https://dl.google.com/android/repository/platform-tools-latest-{os}.zip"), " adb")?;
    let path = dir.join("platform-tools").join(EXE);
    if path.is_file() {
        ui::ok(&format!("adb 已就绪：{}", path.display()));
        Ok(Adb { path })
    } else {
        Err("下载的压缩包里没有 adb".into())
    }
}

pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Adb {
    fn cmd(&self) -> Command {
        #[allow(unused_mut)] // mutated only on Windows
        let mut c = Command::new(&self.path);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        c
    }

    pub fn run(&self, args: &[&str]) -> Output {
        match self.cmd().args(args).stdin(Stdio::null()).output() {
            Ok(o) => Output {
                ok: o.status.success(),
                stdout: String::from_utf8_lossy(&o.stdout).replace('\r', ""),
                stderr: String::from_utf8_lossy(&o.stderr).replace('\r', ""),
            },
            Err(e) => Output { ok: false, stdout: String::new(), stderr: e.to_string() },
        }
    }

    /// `adb shell <cmd>`; output with carriage returns removed.
    pub fn shell(&self, cmd: &str) -> Output {
        self.run(&["shell", cmd])
    }

    /// Runs one subcommand of the router-side helper script.
    pub fn helper(&self, sub: &str) -> Output {
        self.shell(&format!("sh {} {sub}", crate::DEVICE_HELPER))
    }

    pub fn push(&self, local: &Path, remote: &str) -> Result<(), String> {
        let local_s = local.to_string_lossy().to_string();
        let o = self.run(&["push", &local_s, remote]);
        if o.ok { Ok(()) } else { Err(format!("复制到路由器失败（{remote}）：{}", o.stderr.trim())) }
    }

    /// Writes bytes to a temporary file and pushes it.
    pub fn push_bytes(&self, data: &[u8], remote: &str) -> Result<(), String> {
        let mut tmp = std::env::temp_dir();
        let mut rnd = [0u8; 6];
        let _ = getrandom::getrandom(&mut rnd);
        tmp.push(format!("openu60-{}", hex::encode(rnd)));
        std::fs::write(&tmp, data).map_err(|e| format!("无法写临时文件：{e}"))?;
        let r = self.push(&tmp, remote);
        let _ = std::fs::remove_file(&tmp);
        r
    }

    /// Streams `adb exec-out <cmd>` into a reader callback.
    pub fn exec_out<F>(&self, cmd: &str, mut sink: F) -> Result<(), String>
    where
        F: FnMut(&mut dyn Read) -> Result<(), String>,
    {
        let mut child = self
            .cmd()
            .args(["exec-out", cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("无法运行 adb：{e}"))?;
        let mut out = child.stdout.take().unwrap();
        let r = sink(&mut out);
        drop(out);
        let _ = child.wait();
        r
    }

    /// Serial numbers of devices in the "device" state.
    pub fn devices(&self) -> Vec<String> {
        let _ = self.run(&["start-server"]);
        self.run(&["devices"])
            .stdout
            .lines()
            .skip(1)
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let serial = it.next()?;
                (it.next()? == "device").then(|| serial.to_string())
            })
            .collect()
    }

    pub fn connected(&self) -> bool {
        !self.devices().is_empty()
    }
}
