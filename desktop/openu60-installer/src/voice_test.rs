//! Menu 8: voice calls on China-market units (experimental). Shows what the router reads,
//! turns the agent's voice-unlock switch on or off, and saves a test log to send in.
//! Everything goes through the agent on the router (zte-agent/src/voice_unlock.rs).

use serde_json::Value;

use crate::adb::Adb;
use crate::{push_helper, ui, Res};

fn api(a: &Adb, sub: &str) -> Result<Value, String> {
    push_helper(a)?;
    let o = a.helper(sub);
    let text = o.stdout.trim();
    if text.is_empty() {
        return Err("路由器上的 OpenU60 没有回应。请先用第 4 步安装或更新到最新版。".into());
    }
    let v: Value = serde_json::from_str(text).map_err(|_| format!("路由器回复看不懂：{}", text.chars().take(200).collect::<String>()))?;
    if v["ok"].as_bool() != Some(true) {
        let err = v["error"].as_str().unwrap_or("");
        if err.contains("not found") {
            return Err("路由器上的 OpenU60 版本太旧，没有这个功能。请先用第 4 步更新。".into());
        }
        return Err(format!("路由器报错：{err}"));
    }
    Ok(v["data"].clone())
}

fn describe(v: &Value) {
    let m = &v["modem"];
    if let Some(fw) = v["firmware"].as_str().filter(|s| !s.is_empty()) {
        println!("    固件版本：{fw}");
    }
    match m["state"].as_str() {
        Some(state) if state != "error" => {
            let sms = match m["sms_only"].as_u64() {
                Some(b) => format!("{b:02x}"),
                None => "（没有这一项）".into(),
            };
            println!("    sms_only：{sms}");
            println!("    NV5280 前 4 字节：{}", m["nv5280_head"].as_str().unwrap_or("?"));
            match state {
                "unlocked" => ui::ok("可以打电话（两项都是正常值），不需要解锁。"),
                "locked" => ui::warn("被锁成「只能发短信」：这就是国行机打不了电话的原因。"),
                _ => ui::warn(&format!("读到的值不认识，程序不会去改：{}", m["detail"].as_str().unwrap_or(""))),
            }
        }
        _ => ui::warn(&format!("读不到基带：{}", m["detail"].as_str().unwrap_or("未知原因"))),
    }
    println!("    通话解锁开关：{}", if v["enabled"].as_bool() == Some(true) { "开" } else { "关" });
    if let Some(r) = v["last_result"].as_str().filter(|s| !s.is_empty()) {
        println!("    上次检查：{r}");
    }
}

fn status(a: &Adb) -> Res {
    ui::info("正在读取（大约 5 秒）……");
    describe(&api(a, "vu-status")?);
    Ok(())
}

fn enable(a: &Adb) -> Res {
    let v = api(a, "vu-status")?;
    describe(&v);
    if v["modem"]["state"].as_str() != Some("locked") {
        ui::info("这台机器没有被锁成只能短信，开了也不会改任何东西。");
        if !ui::confirm("仍然打开开关吗？（以后换卡被锁时会自动处理）", false) {
            return Ok(());
        }
    } else {
        println!();
        ui::warn("这是实验功能，会改基带里的两项设置（只改那两项的锁定位）。");
        ui::warn("改完会读回来核对，核对通过后路由器会自动重启一次；核对不通过就不重启。");
        ui::warn("测试前请确认已经做过第 2 步备份。");
        if !ui::confirm("确定要打开国行通话解锁吗？", false) {
            return Ok(());
        }
    }
    api(a, "vu-on")?;
    ui::ok("已打开。路由器正在检查，如果需要会改好并自动重启（重启要 1–2 分钟）。");
    ui::info("重启完再用第 8 步 → 1 看一下状态，然后试着打个电话。");
    ui::info("不管成没成功，都请用第 8 步 → 4 收集测试日志，发给作者。");
    Ok(())
}

fn disable(a: &Adb) -> Res {
    api(a, "vu-off")?;
    ui::ok("已关闭。以后不会再自动改基带；已经改过的值保持不变。");
    Ok(())
}

fn collect(a: &Adb) -> Res {
    ui::info("正在收集（大约 10 秒）……");
    let mut out = String::new();
    out.push_str(&format!("OpenU60 国行通话解锁测试日志\n安装工具 {}\n\n", crate::VERSION));
    match api(a, "vu-status") {
        Ok(v) => out.push_str(&format!("== 当前状态\n{}\n\n", serde_json::to_string_pretty(&v).unwrap_or_default())),
        Err(e) => out.push_str(&format!("== 当前状态\n读取失败：{e}\n\n")),
    }
    match api(a, "vu-log") {
        Ok(v) => {
            out.push_str(&format!("== 设置\n{}\n\n", serde_json::to_string_pretty(&v["settings"]).unwrap_or_default()));
            out.push_str("== 检查记录（每行一次）\n");
            out.push_str(v["log"].as_str().unwrap_or(""));
            out.push('\n');
        }
        Err(e) => out.push_str(&format!("== 检查记录\n读取失败：{e}\n")),
    }
    out.push_str("\n== 路由器程序日志（只含通话解锁相关行）\n");
    out.push_str(&a.helper("vu-agentlog").stdout);
    let dir = crate::backup::home().join("OpenU60-测试日志");
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建文件夹：{e}"))?;
    let file = dir.join(format!("国行通话解锁-{}.txt", crate::backup::stamp()));
    std::fs::write(&file, out).map_err(|e| format!("无法保存：{e}"))?;
    ui::ok(&format!("已保存：{}", file.display()));
    ui::info("把这个文件发给作者就行。里面没有手机号、卡号（换卡只记录一个打乱过的编号）。");
    Ok(())
}

pub fn menu(a: &Adb) -> Res {
    loop {
        println!();
        println!("国行机通话解锁（实验，给测试的朋友用）：");
        println!("  1. 查看状态（只读）");
        println!("  2. 打开通话解锁");
        println!("  3. 关闭通话解锁");
        println!("  4. 收集测试日志（保存到电脑，发给作者）");
        println!("  0. 返回");
        let r = match ui::prompt("请输入数字后回车：").as_str() {
            "1" => status(a),
            "2" => enable(a),
            "3" => disable(a),
            "4" => collect(a),
            "0" | "q" | "" => return Ok(()),
            _ => {
                ui::warn("请输入 0–4 之间的数字");
                continue;
            }
        };
        if let Err(e) = r {
            ui::fail(&e);
        }
    }
}
