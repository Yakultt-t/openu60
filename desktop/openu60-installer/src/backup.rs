//! Backups, same layout as the Mac app: identity/partitions, every UCI setting, the /etc
//! overlay, OpenU60's own data, and optionally raw images of the whole eMMC.

use md5::{Digest, Md5};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::adb::Adb;
use crate::ui;

fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Timestamp in UTC, e.g. 20261111-0812 (no date crate needed).
fn stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}", rem / 3600, rem % 3600 / 60)
}

fn out_dir(given: Option<&str>, kind: &str) -> Result<PathBuf, String> {
    let dir = match given {
        Some(d) => PathBuf::from(d),
        None => home().join("OpenU60-备份").join(format!("{}-{kind}", stamp())),
    };
    fs::create_dir_all(&dir).map_err(|e| format!("无法创建文件夹 {}：{e}", dir.display()))?;
    // The backup holds the agent password and keys: owner-only on macOS/Linux
    // (on Windows the user profile is already private).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

fn save_stream(a: &Adb, cmd: &str, path: &Path) -> Result<u64, String> {
    let mut f = File::create(path).map_err(|e| format!("无法写入 {}：{e}", path.display()))?;
    let mut n = 0;
    a.exec_out(cmd, |r| {
        n = std::io::copy(r, &mut f).map_err(|e| format!("读取失败：{e}"))?;
        Ok(())
    })?;
    Ok(n)
}

fn common(a: &Adb, dir: &Path, total: usize) -> Result<(), String> {
    crate::push_helper(a)?;
    ui::step(1, total, "读取设备信息和分区表");
    fs::write(dir.join("identity.txt"), a.helper("identity").stdout).map_err(|e| e.to_string())?;
    ui::step(2, total, "导出全部系统配置");
    let uci = a.shell("uci export");
    if uci.stdout.len() < 100 {
        return Err("导出配置失败".into());
    }
    fs::write(dir.join("uci-export-all.txt"), uci.stdout).map_err(|e| e.to_string())?;
    ui::step(3, total, "打包 /etc 持久分区");
    let n = save_stream(a, "tar -C /zteoverlay -cf - . 2>/dev/null", &dir.join("zteoverlay.tar"))?;
    if n < 10_240 {
        return Err("打包持久分区失败".into());
    }
    ui::step(4, total, "打包 OpenU60 数据");
    save_stream(
        a,
        "cd /data && tar -cf - openu60 local/tmp/*.sh local/tmp/*.json zte-agent-push.json zte-agent-apns.json apns.p8 2>/dev/null",
        &dir.join("agent-data.tar"),
    )?;
    Ok(())
}

pub fn config(a: &Adb, given: Option<&str>) -> Result<(), String> {
    let dir = out_dir(given, "配置")?;
    common(a, &dir, 4)?;
    ui::ok(&format!("配置备份已保存到：{}", dir.display()));
    Ok(())
}

const CHUNK_MB: u64 = 256;
const DEVICES: [&str; 3] = ["mmcblk0boot0", "mmcblk0boot1", "mmcblk0"];

fn dev_size(a: &Adb, dev: &str) -> Result<u64, String> {
    let s = a.shell(&format!("cat /sys/block/{dev}/size")).stdout;
    s.trim().parse::<u64>().map(|n| n * 512).map_err(|_| format!("读不到 {dev} 的大小"))
}

/// Reads one block device in 256 MB chunks: the router gzips each chunk on the fly (much of
/// the eMMC is empty), we write it at its offset, and both sides compare MD5 per chunk, so a
/// bad chunk is re-read instead of the whole 7 GB.
fn dump(a: &Adb, dev: &str, img: &Path, base: u64, grand: u64, started: Instant, manifest: &mut File) -> Result<String, String> {
    let size = dev_size(a, dev)?;
    let chunk = CHUNK_MB * 1024 * 1024;
    let per = CHUNK_MB / 4; // 4 MB blocks per chunk
    let chunks = size.div_ceil(chunk);
    let mut f = OpenOptions::new().create(true).write(true).truncate(true).open(img).map_err(|e| format!("无法写入 {}：{e}", img.display()))?;
    let mut whole = Md5::new();
    for i in 0..chunks {
        let dd = format!("dd if=/dev/{dev} bs=4M skip={} count={per} 2>/dev/null", i * per);
        let mut tries = 0;
        loop {
            tries += 1;
            let want = a.shell(&format!("{dd} | md5sum")).stdout.split_whitespace().next().unwrap_or("").to_string();
            if want.len() != 32 {
                if !a.connected() {
                    return Err("USB 连接断开了，请插好线后重新备份".into());
                }
                return Err(format!("读取 {dev} 失败，请插好线后重新备份"));
            }
            f.seek(SeekFrom::Start(i * chunk)).map_err(|e| e.to_string())?;
            let mut part = Md5::new();
            let mut attempt = whole.clone();
            let mut written = 0u64;
            a.exec_out(&format!("{dd} | gzip -1"), |r| {
                let mut gz = flate2::read::GzDecoder::new(r);
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let n = gz.read(&mut buf).map_err(|e| format!("传输出错：{e}"))?;
                    if n == 0 {
                        break;
                    }
                    f.write_all(&buf[..n]).map_err(|e| format!("写入失败（磁盘满了？）：{e}"))?;
                    part.update(&buf[..n]);
                    attempt.update(&buf[..n]);
                    written += n as u64;
                    ui::progress(base + i * chunk + written, grand, started);
                }
                Ok(())
            })
            .or_else(|e| if tries < 3 { Ok(()) } else { Err(e) })?;
            let got = hex::encode(part.finalize());
            if got == want {
                whole = attempt;
                writeln!(manifest, "{dev} {i} {want}").map_err(|e| e.to_string())?;
                break;
            }
            if tries >= 3 {
                // The router keeps running while it is read, so a block it is busy writing
                // (logs, the SMS database) can change between the two reads.
                println!("\n    注意：{dev} 第 {} 块在读取时一直在被路由器写入，已保存最后一次读到的内容", i + 1);
                whole = attempt;
                writeln!(manifest, "{dev} {i} changed-during-read:{got}").map_err(|e| e.to_string())?;
                break;
            }
            println!("\n    {dev} 第 {} 块校验不一致（路由器正在写入），重新读取（第 {tries} 次）", i + 1);
        }
    }
    f.set_len(size).map_err(|e| e.to_string())?;
    let have = fs::metadata(img).map(|m| m.len()).unwrap_or(0);
    if have != size {
        return Err(format!("{dev} 镜像大小不对（{have} / {size} 字节）"));
    }
    Ok(hex::encode(whole.finalize()))
}

pub fn full(a: &Adb, given: Option<&str>) -> Result<(), String> {
    let dir = out_dir(given, "完整")?;
    ui::info(&format!("保存到：{}（需要约 8 GB 空间）", dir.display()));
    common(a, &dir, 6)?;
    ui::step(5, 6, "读取整块存储芯片（约 7.6 GB，分块校验）");
    let mut grand = 0;
    for dev in DEVICES {
        grand += dev_size(a, dev)?;
    }
    let mut manifest = File::create(dir.join("chunk-md5.txt")).map_err(|e| e.to_string())?;
    writeln!(manifest, "# device chunk({CHUNK_MB}MB) md5").map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut base = 0;
    let mut sums = String::new();
    for dev in DEVICES {
        println!("\n    正在读取 /dev/{dev}");
        let img = dir.join(format!("{dev}.img"));
        let md5 = dump(a, dev, &img, base, grand, started, &mut manifest)?;
        sums.push_str(&format!("{md5}  {dev}.img\n"));
        base += fs::metadata(&img).map(|m| m.len()).unwrap_or(0);
    }
    println!();
    ui::step(6, 6, "写入整体校验值");
    fs::write(dir.join("image-md5.txt"), sums).map_err(|e| e.to_string())?;
    ui::ok(&format!("完整备份完成，每一块都已校验：{}", dir.display()));
    Ok(())
}
