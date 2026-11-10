//! What the screen shows, filled from zte-agent (or mock data in previews).

#[derive(Clone, Default)]
pub struct Status {
    pub hour: u32,
    pub minute: u32,
    pub month: u32,
    pub day: u32,
    pub weekday: u32, // 0 = Sunday
    pub carrier: String,
    pub net_type: String, // "4G", "5G", "无服务"
    pub band: String,     // "B3", "N78"
    pub bandwidth_mhz: f32,
    pub rsrp: f32,
    pub sinr: f32,
    pub bars: u8, // 0..=4
    pub roaming: bool,
    pub down_bps: f64,
    pub up_bps: f64,
    pub down_hist: Vec<f32>,
    pub up_hist: Vec<f32>,
    pub today_bytes: f64,
    pub month_bytes: f64,
    pub plan_bytes: Option<f64>,
    pub clients: u32,
    pub clients_band: String, // "5G" / "2.4G" / "混合"
    pub battery: u8,
    pub charging: bool,
    pub battery_hours: Option<f32>,
    pub memory_pct: f32,
    pub cpu_pct: f32,
    pub temp_c: f32,
    pub unread_sms: u32,
    pub missed_calls: u32,
    pub paired_phones: u32,
    pub remote_mode: String, // "局域网" / "远程·直连" / "远程·转发" / ""
    /// Uplink over Wi-Fi instead of the cellular network.
    pub wifi_uplink: bool,
    /// The router's internet mode: "hotspot" (SIM), "repeater" (home Wi-Fi + own hotspot),
    /// "client" (home Wi-Fi, own hotspot off).
    pub uplink_mode: String,
    pub wifi_uplink_signal: i32,
    /// Data received from the router at least once.
    pub live: bool,
    // Wi-Fi repeater
    pub relay_ssid: String,
    pub relay_freq: u32,
    pub relay_last_ssid: String,
    pub relay_connecting: bool,
    pub scan: Vec<Net>,
    pub scanning: bool,
    // Charging
    pub battery_temp_c: f32,
    pub battery_watts: f32,
    pub charge_limit_enabled: bool,
    pub charge_limit: u32,
    pub powerbank: bool,
    pub charger_connected: bool,
    // Phone line
    pub call: Option<Call>,
    // Hotspot (for the pairing QR code)
    pub hotspot_ssid: String,
    pub hotspot_key: String,
    pub hotspot_enc: String,
    /// Last action result shown as a toast.
    pub toast: Option<(String, std::time::Instant)>,
    /// Hours from UTC, from the network (NITZ).
    pub tz_hours: f64,
    pub adblock: AdBlock,
    // Mirrored from the screen settings for the drawing code.
    pub speed_bytes: bool,
    pub battery_pct: bool,
    pub push_enabled: bool,
    pub push_devices: u32,
    pub missed: Vec<(String, u64)>,
}

#[derive(Clone, Default)]
pub struct AdBlock {
    pub loaded: bool,
    pub enabled: bool,
    pub active: bool,
    pub paused_until: u64,
    pub queries: u64,
    pub blocked: u64,
    pub rate: f32,
    pub devices: u32,
    pub domains: u64,
    pub updating: bool,
    pub hourly_queries: Vec<f32>,
    pub hourly_blocked: Vec<f32>,
    pub top: Vec<(String, u64)>,
}

#[derive(Clone, Default)]
pub struct Net {
    pub ssid: String,
    pub signal: i32,
    pub secure: bool,
    pub freq: u32,
}

#[derive(Clone, PartialEq)]
pub struct Call {
    pub number: String,
    /// "来电" ringing in, "呼出" dialling, "通话中" connected.
    pub state: &'static str,
    pub since: std::time::Instant,
}

impl Status {
    pub fn mock() -> Self {
        let down: Vec<f32> = (0..60)
            .map(|i| {
                let t = i as f32;
                let base = 420_000.0 + 180_000.0 * (t / 5.0).sin().abs();
                if i == 41 { base * 5.0 } else if i == 42 { base * 2.2 } else { base }
            })
            .collect();
        let up: Vec<f32> = (0..60).map(|i| 40_000.0 + 25_000.0 * ((i as f32) / 3.0).cos().abs()).collect();
        Self {
            hour: 13,
            minute: 49,
            month: 11,
            day: 7,
            weekday: 6,
            carrier: "中国移动".into(),
            net_type: "4G".into(),
            band: "B3".into(),
            bandwidth_mhz: 20.0,
            rsrp: -102.0,
            sinr: 8.0,
            bars: 2,
            roaming: true,
            down_bps: 1_430_000.0,
            up_bps: 60_000.0,
            down_hist: down,
            up_hist: up,
            today_bytes: 6.35e9,
            month_bytes: 28.8e9,
            plan_bytes: Some(100e9),
            clients: 2,
            clients_band: "5G".into(),
            battery: 74,
            charging: false,
            battery_hours: Some(12.0),
            memory_pct: 70.0,
            cpu_pct: 56.0,
            temp_c: 52.0,
            unread_sms: 2,
            missed_calls: 1,
            paired_phones: 2,
            remote_mode: "远程·转发".into(),
            wifi_uplink: true,
            uplink_mode: "repeater".into(),
            wifi_uplink_signal: -56,
            live: true,
            relay_ssid: "Home-5G".into(),
            relay_freq: 5180,
            relay_last_ssid: "Home-5G".into(),
            relay_connecting: false,
            scan: vec![
                Net { ssid: "Home-5G".into(), signal: -52, secure: true, freq: 5180 },
                Net { ssid: "Cafe Free WiFi".into(), signal: -67, secure: false, freq: 2437 },
                Net { ssid: "Office".into(), signal: -74, secure: true, freq: 2412 },
            ],
            scanning: false,
            battery_temp_c: 28.0,
            battery_watts: 1.2,
            charge_limit_enabled: true,
            charge_limit: 80,
            powerbank: false,
            charger_connected: false,
            call: None,
            hotspot_ssid: "U60Pro-8A2F".into(),
            hotspot_key: "12345678".into(),
            hotspot_enc: "psk2".into(),
            toast: None,
            tz_hours: 11.0,
            adblock: AdBlock {
                loaded: true,
                enabled: true,
                active: true,
                paused_until: 0,
                queries: 11548,
                blocked: 1485,
                rate: 0.129,
                devices: 2,
                domains: 103_266,
                updating: false,
                hourly_queries: (0..24).map(|i| [80.0, 60.0, 40.0, 30.0, 20.0, 20.0, 30.0, 120.0, 900.0, 600.0, 300.0, 200.0, 150.0, 180.0, 220.0, 260.0, 300.0, 700.0, 500.0, 350.0, 420.0, 380.0, 260.0, 160.0][i]).collect(),
                hourly_blocked: (0..24).map(|i| [10.0, 8.0, 5.0, 4.0, 2.0, 2.0, 4.0, 20.0, 160.0, 90.0, 40.0, 25.0, 18.0, 22.0, 30.0, 35.0, 40.0, 110.0, 70.0, 45.0, 60.0, 50.0, 30.0, 20.0][i]).collect(),
                top: vec![("adashx.ut.amap.com".into(), 153), ("mtalk.google.com".into(), 98), ("cn-hangzhou.log.aliyuncs.com".into(), 61)],
            },
            speed_bytes: false,
            battery_pct: true,
            push_enabled: true,
            push_devices: 2,
            missed: vec![("138 0013 8000".into(), 0)],
        }
    }
}

/// The three internet modes as shown everywhere (screen and app use the same words).
pub fn mode_name(mode: &str) -> &'static str {
    match mode {
        "repeater" => "Wi-Fi 中继",
        "client" => "连接 Wi-Fi",
        _ => "热点",
    }
}

pub fn weekday_cn(d: u32) -> &'static str {
    ["星期日", "星期一", "星期二", "星期三", "星期四", "星期五", "星期六"][(d % 7) as usize]
}

/// Speed in the unit the user picked: bits (Mbps) or bytes (MB/s).
pub fn speed(bps: f64, bytes: bool) -> (String, &'static str) {
    if !bytes {
        return rate(bps);
    }
    let b = bps / 8.0;
    if b >= 1e6 {
        (format!("{:.1}", b / 1e6), "MB/s")
    } else if b >= 1e3 {
        (format!("{:.0}", b / 1e3), "KB/s")
    } else {
        (format!("{:.0}", b), "B/s")
    }
}

/// "1.4" + "Mbps" style split for big numbers.
pub fn rate(bps: f64) -> (String, &'static str) {
    if bps >= 1e9 {
        (format!("{:.2}", bps / 1e9), "Gbps")
    } else if bps >= 1e6 {
        (format!("{:.1}", bps / 1e6), "Mbps")
    } else if bps >= 1e3 {
        (format!("{:.0}", bps / 1e3), "Kbps")
    } else {
        (format!("{:.0}", bps), "bps")
    }
}

pub fn bytes(b: f64) -> (String, &'static str) {
    let (v, unit) = if b >= 1e12 { (b / 1e12, "TB") } else if b >= 1e9 { (b / 1e9, "GB") } else if b >= 1e6 { (b / 1e6, "MB") } else { (b / 1e3, "KB") };
    // Three significant digits at most: 6.35, 28.8, 100.
    let s = if v >= 100.0 { format!("{v:.0}") } else if v >= 10.0 { format!("{v:.1}") } else { format!("{v:.2}") };
    (s, unit)
}

/// Signal quality word and colour key from RSRP/SINR.
pub fn quality(rsrp: f32, sinr: f32) -> (&'static str, u8) {
    if rsrp >= -90.0 && sinr >= 13.0 {
        ("信号很好", 0)
    } else if rsrp >= -100.0 && sinr >= 5.0 {
        ("信号良好", 1)
    } else if rsrp >= -110.0 {
        ("信号较弱", 2)
    } else {
        ("信号很差", 3)
    }
}
