//! OpenU60 router installer: the command-line counterpart of the Mac app "U60 刷机助手",
//! for Windows (and macOS / Linux). One file: the router payload is embedded at build time.
//!
//!   openu60-installer                 menu (what a double-click opens)
//!   openu60-installer enable-adb [gateway]
//!   openu60-installer status
//!   openu60-installer backup [dir]    configuration backup
//!   openu60-installer backup-full [dir]
//!   openu60-installer install [--new-password]
//!   openu60-installer uninstall

mod adb;
mod backup;
mod ui;
mod web;

use adb::Adb;
use std::collections::HashMap;
use std::time::{Duration, Instant};

include!(concat!(env!("OUT_DIR"), "/payload.rs"));

pub const DEVICE_HELPER: &str = "/data/local/tmp/openu60-device.sh";
const VERSION: &str = env!("CARGO_PKG_VERSION");

type Res = Result<(), String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("OpenU60 路由器安装工具 v{VERSION}（配合 iPhone app「SIM 随行」使用）");
    if args.is_empty() {
        menu();
        return;
    }
    let r = match args[0].as_str() {
        "enable-adb" => enable_adb(args.get(1).map(String::as_str)),
        "status" => with_adb(|a| status(&a)),
        "backup" => with_adb(|a| backup::config(&a, args.get(1).map(String::as_str))),
        "backup-full" => with_adb(|a| backup::full(&a, args.get(1).map(String::as_str))),
        "install" => with_adb(|a| install(&a, args.iter().any(|x| x == "--new-password"), false)),
        "uninstall" => with_adb(|a| uninstall(&a, false)),
        "-h" | "--help" | "help" => {
            println!("用法：openu60-installer [enable-adb [路由器地址] | status | backup [目录] | backup-full [目录] | install [--new-password] | uninstall]");
            Ok(())
        }
        other => Err(format!("不认识的命令：{other}（用 --help 看用法）")),
    };
    if let Err(e) = r {
        ui::fail(&e);
        std::process::exit(1);
    }
}

fn menu() {
    loop {
        println!();
        println!("用 USB 线把中兴 U60 Pro 连到这台电脑，然后按顺序做：");
        println!("  1. 开启 USB 调试（需要路由器后台密码）");
        println!("  2. 备份路由器配置（几秒钟，建议先做）");
        println!("  3. 完整备份整块存储（约 7.6 GB，要半小时左右，可跳过）");
        println!("  4. 安装 / 更新 OpenU60");
        println!("  5. 查看状态");
        println!("  6. 卸载 OpenU60");
        println!("  0. 退出");
        let choice = ui::prompt("请输入数字后回车：");
        let r = match choice.as_str() {
            "1" => enable_adb(None),
            "2" => with_adb(|a| backup::config(&a, None)),
            "3" => with_adb(|a| backup::full(&a, None)),
            "4" => with_adb(|a| install(&a, false, true)),
            "5" => with_adb(|a| status(&a)),
            "6" => with_adb(|a| uninstall(&a, true)),
            "0" | "q" | "" => return,
            _ => {
                ui::warn("请输入 0–6 之间的数字");
                continue;
            }
        };
        if let Err(e) = r {
            ui::fail(&e);
        }
        ui::prompt("\n按回车回到菜单……");
    }
}

/// Finds adb (offering to download Google's), then checks a router is attached.
fn get_adb() -> Result<Adb, String> {
    if let Some(a) = adb::find() {
        return Ok(a);
    }
    ui::warn("这台电脑上没有找到 adb（Google 的安卓调试工具），安装要用到它。");
    if !ui::confirm("现在从 Google 官网下载吗？", true) {
        return Err("没有 adb，无法继续。可以把 adb 放在本程序同一个文件夹里再试。".into());
    }
    adb::download()
}

fn with_adb<F: FnOnce(Adb) -> Res>(f: F) -> Res {
    let a = get_adb()?;
    if !a.connected() {
        return Err("没有连上路由器的 ADB。请插好 USB 线，并先做第 1 步「开启 USB 调试」。".into());
    }
    if a.devices().len() > 1 {
        return Err("这台电脑连着不止一个安卓设备，请只留下 U60 Pro 再试。".into());
    }
    f(a)
}

pub(crate) fn push_helper(a: &Adb) -> Res {
    a.push_bytes(DEVICE_SCRIPT, DEVICE_HELPER)?;
    let _ = a.shell(&format!("chmod 700 {DEVICE_HELPER}"));
    Ok(())
}

fn device_status(a: &Adb) -> Result<HashMap<String, String>, String> {
    push_helper(a)?;
    let o = a.helper("status");
    if !o.ok && o.stdout.is_empty() {
        return Err(format!("读取路由器状态失败：{}", o.stderr.trim()));
    }
    Ok(o.stdout
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect())
}

fn enable_adb(gateway: Option<&str>) -> Res {
    let a = get_adb()?;
    if a.connected() {
        ui::ok("USB 调试已经开着，可以直接做下一步。");
        return Ok(());
    }
    println!("\n这一步用路由器自己的网页后台把 USB 口切到调试模式。");
    println!("要求：电脑连着路由器的 Wi-Fi（或用 USB 线上网），并且 USB 线插着。");
    let gw = match gateway {
        Some(g) => g.to_string(),
        None => ui::prompt_default("路由器地址", "192.168.0.1"),
    };
    let pw = ui::secret("路由器后台密码（输入时不显示，只用这一次，不会保存）：");
    if pw.is_empty() {
        return Err("没有输入密码".into());
    }
    web::enable_debug_mode(&gw, &pw)?;
    drop(pw);
    ui::ok("已切到调试模式，等电脑认出 ADB（最多 40 秒）……");
    let _ = a.run(&["kill-server"]);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(40) {
        if a.connected() {
            ui::ok("ADB 已连上，可以做下一步了。");
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    driver_help();
    Err("电脑没认出 ADB".into())
}

/// On Windows the router's ADB interface (USB\VID_19D2&PID_1404&MI_06) has no driver out of
/// the box: its descriptors do not ask for WinUSB, so Windows leaves it as an unknown device.
fn driver_help() {
    if !cfg!(windows) {
        ui::warn("请换一根能传数据的 USB 线或换一个 USB 口，再运行一次第 1 步。");
        return;
    }
    let ps = "Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like 'USB\\VID_19D2&PID_1404*' } | \
              ForEach-Object { $_.Status + '|' + $_.InstanceId + '|' + $_.FriendlyName }";
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        ui::warn("Windows 没有看到路由器的 USB 设备。请确认 USB 线能传数据（有些线只能充电），换个 USB 口再试。");
        return;
    }
    let adb_iface = lines.iter().find(|l| l.contains("MI_06"));
    if adb_iface.is_some_and(|l| l.starts_with("OK|")) {
        ui::warn("ADB 接口的驱动已经装好，但 adb 还没认出来。请拔插一次 USB 线，再运行第 1 步。");
        return;
    }
    let name = adb_iface.and_then(|l| l.split('|').nth(2)).filter(|n| !n.trim().is_empty()).unwrap_or("ADB Interface");
    ui::warn("Windows 还没有给路由器的 ADB 接口装驱动（只需要装一次）。");
    if !ui::confirm("现在从 Google 官网下载官方 USB 驱动吗？", true) {
        return;
    }
    let inf = match adb::fetch_zip("https://dl.google.com/android/repository/usb_driver_r13-windows.zip", " Google USB 驱动") {
        Ok(dir) => dir.join("usb_driver").join("android_winusb.inf"),
        Err(e) => {
            ui::warn(&e);
            return;
        }
    };
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", "devmgmt.msc"]).spawn();
    ui::info("已打开「设备管理器」，请照着做：");
    ui::info(&format!("1. 在「其他设备」里找到带黄色感叹号的「{name}」，右键 → 更新驱动程序"));
    ui::info("2. 选「浏览我的电脑以查找驱动程序」→「让我从计算机上的可用驱动程序列表中选取」");
    ui::info("3. 选「显示所有设备」→ 下一步 →「从磁盘安装」→「浏览」，打开这个文件：");
    ui::info(&format!("   {}", inf.display()));
    ui::info("4. 选「Android ADB Interface」→ 下一步；弹出警告时点「是」");
    ui::info("5. 装好后拔插一次 USB 线，回到这里再运行第 1 步");
}

fn status(a: &Adb) -> Res {
    let s = device_status(a)?;
    let g = |k: &str| s.get(k).map(String::as_str).unwrap_or("?");
    println!();
    ui::info(&format!("型号：{}    固件：{}", g("model"), g("firmware")));
    ui::info(&format!(
        "OpenU60：{}，{}",
        if g("agent") == "installed" { "已安装" } else { "未安装" },
        if g("agent_running") == "yes" { "正在运行" } else { "没有运行" }
    ));
    ui::info(&format!("开机自启：{}    固件自动升级：{}", if g("boot_hook") == "yes" { "有" } else { "无" }, if g("fota") == "on" { "开（升级会冲掉 OpenU60）" } else { "已关闭" }));
    Ok(())
}

fn new_password() -> String {
    let chars: Vec<char> = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789".chars().collect();
    let mut buf = [0u8; 20];
    getrandom::getrandom(&mut buf).expect("random");
    buf.iter().map(|b| chars[*b as usize % chars.len()]).collect()
}

fn start_script(password: &str) -> String {
    let safe = password.replace('\'', "'\\''");
    format!(
        "#!/bin/sh\n# Starts the zte-agent supervisor detached from whatever shell launched it (adb, rc.local).\n\
         export ZTE_AGENT_PASSWORD='{safe}'\n\
         [ -f /data/zte-agent-apns.json ] && export ZTE_AGENT_LIVE_ACTIVITY_CONFIG=/data/zte-agent-apns.json\n\
         cd /data\nstart-stop-daemon -S -b -x /data/local/tmp/zte-agent-run.sh\n"
    )
}

fn install(a: &Adb, force_new: bool, interactive: bool) -> Res {
    const TOTAL: usize = 6;
    ui::step(1, TOTAL, "检查路由器");
    let s = device_status(a)?;
    let g = |k: &str| s.get(k).cloned().unwrap_or_default();
    if g("arch") != "aarch64" {
        return Err(format!("不支持的处理器（{}），这个版本只适用于中兴 U60 Pro", g("arch")));
    }
    ui::info(&format!("硬件版本：{}", g("model")));
    if g("free_kb").parse::<u64>().unwrap_or(0) < 40_000 {
        return Err("路由器 /data 空间不足（需要约 40 MB）".into());
    }
    let existing = if g("password") == "set" { a.helper("password").stdout.trim().to_string() } else { String::new() };
    let keep = !existing.is_empty()
        && !force_new
        && (!interactive || ui::confirm("路由器上已经有 SIM 随行密码，保留它吗？（保留的话，已配对的手机不用重新登录）", true));
    let password = if keep { existing } else { new_password() };
    if keep {
        ui::info("保留现有的 SIM 随行密码");
    }

    ui::step(2, TOTAL, "关闭固件自动升级和中兴云服务（固件升级会把 OpenU60 冲掉）");
    a.helper("disable-cloud");
    ui::ok("已关闭");

    ui::step(3, TOTAL, "停止正在运行的旧版本");
    a.helper("stop-agent");

    ui::step(4, TOTAL, "复制程序和脚本到路由器");
    let _ = a.shell("rm -rf /data/local/tmp/openu60-new; mkdir -p /data/local/tmp/openu60-new");
    for (name, data) in PAYLOAD {
        ui::info(&format!("{name}（{} KB）", data.len() / 1024));
        a.push_bytes(data, &format!("/data/local/tmp/openu60-new/{name}"))?;
    }
    if !keep {
        a.push_bytes(start_script(&password).as_bytes(), "/data/local/tmp/openu60-new/start_zte_agent.sh")?;
    }
    let o = a.helper("place-files");
    if !o.stdout.contains("ok") {
        return Err(format!("安装文件失败：{}{}", o.stdout.trim(), o.stderr.trim()));
    }
    if o.stdout.contains("screen=installed") {
        ui::ok("已安装新的触摸屏界面（连按三下电源键切换）");
    }

    ui::step(5, TOTAL, "设置开机自动启动");
    if !a.helper("boot-hooks").stdout.contains("ok") {
        return Err("写入开机启动失败".into());
    }
    ui::ok("已写入 /etc/rc.local");

    ui::step(6, TOTAL, "启动并检查");
    a.helper("start");
    let _ = a.run(&["forward", "tcp:19091", "tcp:9090"]);
    let start = Instant::now();
    let mut info = None;
    while start.elapsed() < Duration::from_secs(45) {
        if let Ok(v) = web::agent_login(19091, &password) {
            info = Some(v);
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let _ = a.run(&["forward", "--remove", "tcp:19091"]);
    let Some(info) = info else {
        println!("{}", a.helper("log").stdout);
        return Err("OpenU60 没有响应，上面是它最后的日志".into());
    };
    if let Some(id) = web::find_str(&info, "device_id") {
        ui::info(&format!("设备 ID：{id}"));
    }
    ui::ok("OpenU60 已安装并在运行");
    println!();
    if keep {
        println!("下一步：打开 iPhone 上的「SIM 随行」，用原来的密码就能连上。");
    } else {
        println!("============================================================");
        println!("  SIM 随行密码：{password}");
        println!("============================================================");
        println!("请抄下来保存：它只显示这一次，只存在路由器上。");
        println!("下一步：iPhone 打开「SIM 随行」，按引导连上路由器的 Wi-Fi，输入这个密码完成配对。");
    }
    Ok(())
}

fn uninstall(a: &Adb, interactive: bool) -> Res {
    if interactive && !ui::confirm("确定要卸载 OpenU60 吗？路由器会恢复原厂界面和热点模式", false) {
        return Ok(());
    }
    push_helper(a)?;
    ui::step(1, 1, "停止并删除 OpenU60");
    if !a.helper("uninstall").stdout.contains("ok") {
        return Err("卸载没有完成".into());
    }
    ui::ok("已卸载。/data/openu60 里的数据保留；固件自动升级仍是关闭的。");
    Ok(())
}
