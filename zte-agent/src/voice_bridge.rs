//! Call-audio bridge between the modem DSP and the phone app.
//!
//! The firmware ships ZTE's "ODU" build of `zte_route_hal_voice_test`. Started with
//! `-p dummy -r dummy -t 0` it opens the voice graph for the active call and
//!   * sends the downlink (far end) as G.711 µ-law RTP from 192.168.253.1:48894 to
//!     192.168.253.100:48894, 160 samples (20 ms) per packet;
//!   * plays RTP received on 192.168.253.1:48894 (source must be 192.168.253.100) into
//!     the uplink.
//! Both addresses are hard-coded in the tool, so the bridge puts them on `lo` and sits at
//! 192.168.253.100. The tool only attaches to a call when it is started AFTER the call is
//! active (started earlier it never gets DSP data), so a monitor thread polls `AT+CLCC`
//! and starts it on the first `active`, kills it when the call is gone. While an outgoing
//! call rings it also tries the tool briefly, to pass on the network's own ringback (彩铃,
//! "the number you dialled is switched off") if the DSP carries early media; that instance
//! is always replaced once the call is answered.
//!
//! Phone side: one UDP port (default 9091). Every datagram, in both directions, is
//!   "U60A" | type u8 | flags u8 | seq u16 BE | ts u32 BE | key[16] | payload
//! type 0 = 20 ms µ-law audio (160 B payload), 1 = hello/keepalive (echoed back),
//! 2 = bye. The key is handed out by POST /api/call/audio/start (which needs the bearer
//! token); datagrams with a wrong key are dropped. The phone's address is learned from
//! its datagrams, so it can sit behind NAT or come in over Tailscale.
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::at_cmd::{self, AtPort};
use crate::telephony::parse_clcc;

const ODU_BIN: &str = "/usr/bin/zte_route_hal_voice_test";
/// Address the ODU tool binds (RTP source and uplink sink).
const ODU_ADDR: Ipv4Addr = Ipv4Addr::new(192, 168, 253, 1);
/// Address the tool sends the downlink to and expects the uplink from (us).
const IDU_ADDR: Ipv4Addr = Ipv4Addr::new(192, 168, 253, 100);
const RTP_PORT: u16 = 48894;
const DEFAULT_PHONE_PORT: u16 = 9091;
const DEFAULT_VSID: &str = "11C05000";
/// PAL device pair that opens on this unit (speaker + built-in mic ids).
const DEFAULT_DEVICES: &str = "2,2147483652";

const MAGIC: [u8; 4] = *b"U60A";
const HDR_LEN: usize = 28;
const TYPE_AUDIO: u8 = 0;
const TYPE_HELLO: u8 = 1;
const TYPE_BYE: u8 = 2;
const RTP_SSRC: u32 = 0x5a5a_1234;

const POLL_INTERVAL: Duration = Duration::from_millis(400);
/// The voice tool only attaches to a call that is already up at the modem; give the
/// call this long after the first "active" before starting it.
const START_DELAY: Duration = Duration::from_millis(1200);
/// If no downlink packets arrive from the DSP this long after a start, the tool did
/// not attach: restart it (ZTE's own scripts restart it on every hook event).
const ATTACH_TIMEOUT: Duration = Duration::from_millis(3500);
const ATTACH_MIN_PACKETS: u64 = 25;
/// An outgoing call still "dialing" after this long may already be playing an
/// announcement as early media: try the tool then, not only once it is "alerting".
const DIALING_PROBE_AFTER: Duration = Duration::from_millis(2500);
/// Tries per call while ringing; failed ones cost the active call nothing.
const EARLY_TRIES: u32 = 2;
/// Packets whose mean level reaches this (of 32124) carry sound rather than DSP silence
/// or comfort noise: about -44 dBFS.
const LOUD_LEVEL: u32 = 200;
/// End the session this long after the modem stops listing any voice call.
const IDLE_END: Duration = Duration::from_secs(8);
/// Safety cap: a session never outlives this (a lost phone must not keep the DSP open).
const MAX_SESSION: Duration = Duration::from_secs(4 * 3600);
/// Downlink forwarding is capped only to stop the pathological idle spin (the unattached
/// tool emits thousands of silent packets per second). A real call is bursty — the DSP
/// hands over several 20 ms frames at once — so the cap sits well above the 50 packets/s
/// of live audio and lets every real burst through; the phone's jitter buffer paces playout.
/// Refill is one token per `REFILL_US`; `MAX_BURST` is the bucket depth.
const MAX_BURST: u32 = 100;
const REFILL_US: u128 = 5_000;

/// Uplink pacing. The DSP tool plays whatever arrives straight into the call with no jitter
/// buffer of its own, so phone frames must reach it every 20 ms like clockwork. The phone's
/// capture callback bunches frames and the network (the relay path above all) adds jitter, so
/// frames are parked here briefly and clocked out by a pacer; a hole is filled with silence
/// rather than heard as a stutter at the far end.
const UP_FRAME: Duration = Duration::from_millis(20);
const UP_START_TARGET: usize = 2; // 40 ms
const UP_MIN_TARGET: usize = 2; // 40 ms
const UP_MAX_TARGET: usize = 6; // 120 ms
/// Frames beyond target + this are dropped: latency must not creep up when the phone's clock
/// runs a little fast or a burst lands after a stall.
const UP_SLACK: usize = 2;
/// 8 s without an underrun: try one frame less of delay.
const UP_SHRINK_TICKS: u32 = 400;
/// No phone audio for this long: drop what is queued and send silence until it is back.
const UP_IDLE: Duration = Duration::from_secs(2);
const ULAW_SILENCE: u8 = 0xFF;
/// A different phone endpoint may take the session over after the current one is quiet this long.
const OWNER_TAKEOVER: Duration = Duration::from_millis(2500);

/// Persisted choice of how the modem's voice graph is opened (see `MicMode`).
const SETTINGS_PATH: &str = "/data/openu60/voice.json";

/// The ODU tool opens a PAL device pair; by default that includes the router's own
/// microphone, which the far end then hears on top of the phone's audio. `MuteTx` starts it
/// with `-u 0` (transmit muted at the DSP), `NoMic` opens the speaker only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MicMode {
    Default,
    MuteTx,
    NoMic,
}

impl MicMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "default" | "builtin" => Some(Self::Default),
            "mute_tx" => Some(Self::MuteTx),
            "no_mic" => Some(Self::NoMic),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::MuteTx => "mute_tx",
            Self::NoMic => "no_mic",
        }
    }
}

/// Everything the monitor needs to launch the tool.
#[derive(Clone, Debug)]
struct OduLaunch {
    vsid: String,
    devices: String,
    mute_tx: bool,
}

pub struct VoiceBridge {
    session: Mutex<Option<Arc<Session>>>,
    at_port: Arc<AtPort>,
    phone_port: u16,
    vsid: String,
    devices: String,
    mic_mode: Mutex<MicMode>,
}

struct Session {
    key: [u8; 16],
    started: Instant,
    stop: AtomicBool,
    phone: Mutex<Option<SocketAddr>>,
    phone_seen: Mutex<Option<Instant>>,
    phone_sock: UdpSocket,
    dsp_sock: UdpSocket,
    odu_running: AtomicBool,
    odu_starts: AtomicU64,
    call_state: Mutex<String>,
    rx_phone: AtomicU64,
    tx_phone: AtomicU64,
    rx_dsp: AtomicU64,
    /// DSP packets with sound in them (`LOUD_LEVEL`), for telling early media from silence.
    dsp_loud: AtomicU64,
    dropped: AtomicU64,
    bad_key: AtomicU64,
    ended: AtomicBool,
    uplink: Mutex<Uplink>,
    up_underruns: AtomicU64,
    up_dropped: AtomicU64,
    up_silence: AtomicU64,
    /// Packets ignored because another phone owns the session.
    foreign: AtomicU64,
}

struct Uplink {
    frames: VecDeque<Vec<u8>>,
    target: usize,
    primed: bool,
    ticks_since_underrun: u32,
    /// Consecutive pacer ticks with nothing to send.
    empty_ticks: u32,
    last_rx: Option<Instant>,
}

impl VoiceBridge {
    pub fn new() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let mic_mode = std::fs::read_to_string(SETTINGS_PATH)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v["mic_mode"].as_str().and_then(MicMode::parse))
            .unwrap_or(MicMode::Default);
        Self {
            session: Mutex::new(None),
            at_port: Arc::new(AtPort::new()),
            phone_port: env("ZTE_VOICE_PORT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_PHONE_PORT),
            vsid: env("ZTE_VOICE_VSID").unwrap_or_else(|| DEFAULT_VSID.to_string()),
            devices: env("ZTE_VOICE_DEVICES").unwrap_or_else(|| DEFAULT_DEVICES.to_string()),
            mic_mode: Mutex::new(mic_mode),
        }
    }

    pub fn mic_mode(&self) -> &'static str {
        self.mic_mode.lock().unwrap().as_str()
    }

    /// Takes effect on the next audio session (the tool is started per call).
    pub fn set_mic_mode(&self, mode: &str) -> Result<Value, String> {
        let parsed = MicMode::parse(mode).ok_or("mic_mode must be default, mute_tx or no_mic")?;
        *self.mic_mode.lock().unwrap() = parsed;
        let _ = std::fs::create_dir_all("/data/openu60");
        std::fs::write(SETTINGS_PATH, json!({"mic_mode": parsed.as_str()}).to_string())
            .map_err(|e| format!("save: {e}"))?;
        Ok(json!({"mic_mode": parsed.as_str()}))
    }

    fn launch(&self) -> OduLaunch {
        let mode = *self.mic_mode.lock().unwrap();
        OduLaunch {
            vsid: self.vsid.clone(),
            devices: if mode == MicMode::NoMic { "2".to_string() } else { self.devices.clone() },
            mute_tx: mode == MicMode::MuteTx,
        }
    }

    /// Start a session (or return the running one). Called by the dial/answer handlers
    /// and by the app; the session itself waits for the call to become active.
    pub fn start(&self) -> Result<Value, String> {
        let mut slot = self.session.lock().unwrap();
        if let Some(s) = slot.as_ref() {
            if !s.ended.load(Ordering::Relaxed) {
                return Ok(self.describe(s, false));
            }
        }
        // Drop the finished session so its sockets close; the relay threads of that
        // session may hold them for up to one read timeout, hence the bind retries.
        *slot = None;
        ensure_lo_addresses();
        kill_stray_odu();
        let (phone_sock, dsp_sock) = bind_with_retry(self.phone_port)?;
        for s in [&phone_sock, &dsp_sock] {
            s.set_read_timeout(Some(Duration::from_millis(300)))
                .map_err(|e| format!("socket timeout: {e}"))?;
        }
        let session = Arc::new(Session {
            key: rand::random(),
            started: Instant::now(),
            stop: AtomicBool::new(false),
            phone: Mutex::new(None),
            phone_seen: Mutex::new(None),
            phone_sock,
            dsp_sock,
            odu_running: AtomicBool::new(false),
            odu_starts: AtomicU64::new(0),
            call_state: Mutex::new("unknown".to_string()),
            rx_phone: AtomicU64::new(0),
            tx_phone: AtomicU64::new(0),
            rx_dsp: AtomicU64::new(0),
            dsp_loud: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            bad_key: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            uplink: Mutex::new(Uplink {
                frames: VecDeque::with_capacity(32),
                target: UP_START_TARGET,
                primed: false,
                ticks_since_underrun: 0,
                empty_ticks: 0,
                last_rx: None,
            }),
            up_underruns: AtomicU64::new(0),
            up_dropped: AtomicU64::new(0),
            up_silence: AtomicU64::new(0),
            foreign: AtomicU64::new(0),
        });
        let s = Arc::clone(&session);
        crate::system::spawn_named("voice-phone-rx", move || s.phone_to_dsp());
        let s = Arc::clone(&session);
        crate::system::spawn_named("voice-dsp-rx", move || s.dsp_to_phone());
        let s = Arc::clone(&session);
        crate::system::spawn_named("voice-uplink", move || s.uplink_pacer());
        let s = Arc::clone(&session);
        let at_port = Arc::clone(&self.at_port);
        let launch = self.launch();
        crate::system::spawn_named("voice-monitor", move || s.monitor(at_port, launch));
        eprintln!("[voice] session started (udp {})", self.phone_port);
        let v = self.describe(&session, true);
        *slot = Some(session);
        Ok(v)
    }

    pub fn stop(&self) -> Value {
        let slot = self.session.lock().unwrap();
        match slot.as_ref() {
            Some(s) if !s.ended.load(Ordering::Relaxed) => {
                s.stop.store(true, Ordering::Relaxed);
                json!({"stopped": true})
            }
            _ => json!({"stopped": false}),
        }
    }

    pub fn status(&self) -> Value {
        let slot = self.session.lock().unwrap();
        match slot.as_ref() {
            Some(s) if !s.ended.load(Ordering::Relaxed) => {
                let mut v = self.describe(s, false);
                let phone = s.phone.lock().unwrap().map(|a| a.to_string());
                let seen_ago = s.phone_seen.lock().unwrap().map(|t| t.elapsed().as_secs_f64());
                v["active"] = json!(true);
                v["uptime_s"] = json!(s.started.elapsed().as_secs());
                v["call_state"] = json!(*s.call_state.lock().unwrap());
                v["odu_running"] = json!(s.odu_running.load(Ordering::Relaxed));
                v["odu_starts"] = json!(s.odu_starts.load(Ordering::Relaxed));
                v["phone"] = json!(phone);
                v["phone_seen_ago_s"] = json!(seen_ago);
                v["rx_phone"] = json!(s.rx_phone.load(Ordering::Relaxed));
                v["tx_phone"] = json!(s.tx_phone.load(Ordering::Relaxed));
                v["rx_dsp"] = json!(s.rx_dsp.load(Ordering::Relaxed));
                v["dsp_loud"] = json!(s.dsp_loud.load(Ordering::Relaxed));
                v["dropped"] = json!(s.dropped.load(Ordering::Relaxed));
                v["bad_key"] = json!(s.bad_key.load(Ordering::Relaxed));
                {
                    let u = s.uplink.lock().unwrap();
                    v["uplink_depth_ms"] = json!(u.frames.len() * 20);
                    v["uplink_target_ms"] = json!(u.target * 20);
                }
                v["uplink_underruns"] = json!(s.up_underruns.load(Ordering::Relaxed));
                v["uplink_dropped"] = json!(s.up_dropped.load(Ordering::Relaxed));
                v["uplink_silence"] = json!(s.up_silence.load(Ordering::Relaxed));
                v["mic_mode"] = json!(self.mic_mode());
                v["foreign"] = json!(s.foreign.load(Ordering::Relaxed));
                v
            }
            _ => json!({"active": false, "mic_mode": self.mic_mode()}),
        }
    }

    fn describe(&self, s: &Session, new: bool) -> Value {
        json!({
            "port": self.phone_port,
            "key": hex(&s.key),
            "codec": "pcmu",
            "frame_ms": 20,
            "sample_rate": 8000,
            "new": new,
        })
    }
}

impl Session {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.ended.load(Ordering::Relaxed)
    }

    /// Phone -> uplink buffer: validate, learn the phone's address, park the frame for the pacer.
    fn phone_to_dsp(&self) {
        raise_audio_priority();
        let mut buf = [0u8; 2048];
        while !self.stopping() {
            let (n, from) = match self.phone_sock.recv_from(&mut buf) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => {
                    eprintln!("[voice] phone socket error: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            if n < HDR_LEN || buf[..4] != MAGIC {
                continue;
            }
            if buf[12..28] != self.key {
                self.bad_key.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            {
                // One phone owns the session. Another endpoint only takes over once the owner
                // has been silent for a while (it moved to a new tunnel or path); before that
                // its packets are ignored, so two devices never interleave audio.
                let mut phone = self.phone.lock().unwrap();
                let mut seen = self.phone_seen.lock().unwrap();
                let owner_alive = seen.map(|t| t.elapsed() < OWNER_TAKEOVER).unwrap_or(false);
                if *phone != Some(from) {
                    if phone.is_some() && owner_alive {
                        self.foreign.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    eprintln!("[voice] phone endpoint {from}");
                    *phone = Some(from);
                }
                *seen = Some(Instant::now());
            }
            self.rx_phone.fetch_add(1, Ordering::Relaxed);
            match buf[4] {
                TYPE_AUDIO => {
                    let payload = &buf[HDR_LEN..n];
                    if payload.is_empty() || payload.len() > 1024 {
                        continue;
                    }
                    let mut u = self.uplink.lock().unwrap();
                    u.last_rx = Some(Instant::now());
                    u.frames.push_back(payload.to_vec());
                    // Latency cap: a phone clock that runs fast, or a burst after a stall,
                    // must not pile up behind the pacer.
                    let cap = u.target + UP_SLACK;
                    while u.frames.len() > cap {
                        u.frames.pop_front();
                        self.up_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                TYPE_HELLO => {
                    // Echo so the app can measure reachability / round trip.
                    let mut reply = [0u8; HDR_LEN];
                    reply[..n.min(HDR_LEN)].copy_from_slice(&buf[..n.min(HDR_LEN)]);
                    let _ = self.phone_sock.send_to(&reply, from);
                }
                TYPE_BYE => {
                    *self.phone.lock().unwrap() = None;
                }
                _ => {}
            }
        }
    }

    /// Uplink buffer -> DSP: one RTP frame every 20 ms on a steady clock. Waits for `target`
    /// frames before starting (and after every underrun, which also deepens the buffer);
    /// a missing frame goes out as silence so the far end hears a tiny gap, not a glitch.
    fn uplink_pacer(&self) {
        raise_audio_priority();
        let mut seq: u16 = rand::random();
        let mut ts: u32 = rand::random();
        let dst = SocketAddr::V4(SocketAddrV4::new(ODU_ADDR, RTP_PORT));
        let mut rtp = [0u8; 12 + 1024];
        let silence = vec![ULAW_SILENCE; 160];
        let mut next = Instant::now();
        while !self.stopping() {
            next += UP_FRAME;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > Duration::from_millis(200) {
                // The thread was starved; do not try to catch up with a burst.
                next = now;
            }
            let frame: Option<Vec<u8>> = {
                let mut u = self.uplink.lock().unwrap();
                if u.last_rx.map(|t| t.elapsed() > UP_IDLE).unwrap_or(true) {
                    // The phone is not sending (path change, app suspended): keep clocking
                    // silence into the call. When the RTP stream stopped, the voice graph fell
                    // back to the router's own microphone and the far end heard the room.
                    u.primed = false;
                    u.frames.clear();
                    Some(silence.clone())
                } else if !u.primed {
                    if u.frames.len() >= u.target {
                        u.primed = true;
                        u.ticks_since_underrun = 0;
                        u.frames.pop_front()
                    } else {
                        Some(silence.clone())
                    }
                } else {
                    match u.frames.pop_front() {
                        Some(f) => {
                            u.ticks_since_underrun += 1;
                            u.empty_ticks = 0;
                            if u.ticks_since_underrun >= UP_SHRINK_TICKS {
                                u.ticks_since_underrun = 0;
                                u.target = (u.target - 1).max(UP_MIN_TARGET);
                            }
                            Some(f)
                        }
                        None => {
                            // Send silence for the frame that is missing and keep the clock
                            // running; the next frame plays as soon as it lands. Only a real
                            // stall (10 empty frames) goes back to buffering up to the target.
                            u.empty_ticks += 1;
                            if u.empty_ticks == 1 {
                                u.target = (u.target + 1).min(UP_MAX_TARGET);
                                u.ticks_since_underrun = 0;
                                self.up_underruns.fetch_add(1, Ordering::Relaxed);
                            }
                            if u.empty_ticks >= 10 {
                                u.primed = false;
                                u.empty_ticks = 0;
                            }
                            Some(silence.clone())
                        }
                    }
                }
            };
            let Some(frame) = frame else { continue };
            if frame[0] == ULAW_SILENCE && frame.iter().all(|&b| b == ULAW_SILENCE) {
                self.up_silence.fetch_add(1, Ordering::Relaxed);
            }
            rtp[0] = 0x80;
            rtp[1] = 0; // PT 0 = PCMU, no marker
            rtp[2..4].copy_from_slice(&seq.to_be_bytes());
            rtp[4..8].copy_from_slice(&ts.to_be_bytes());
            rtp[8..12].copy_from_slice(&RTP_SSRC.to_be_bytes());
            rtp[12..12 + frame.len()].copy_from_slice(&frame);
            let _ = self.dsp_sock.send_to(&rtp[..12 + frame.len()], dst);
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(frame.len() as u32);
        }
    }

    /// DSP -> phone: strip the RTP header, rate-limit, forward to the last phone address.
    fn dsp_to_phone(&self) {
        raise_audio_priority();
        let mut buf = [0u8; 2048];
        let mut out = [0u8; HDR_LEN + 1024];
        out[..4].copy_from_slice(&MAGIC);
        out[4] = TYPE_AUDIO;
        out[12..28].copy_from_slice(&self.key);
        let mut tokens: u32 = MAX_BURST;
        let mut last_refill = Instant::now();
        while !self.stopping() {
            let n = match self.dsp_sock.recv_from(&mut buf) {
                Ok((n, _)) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => {
                    eprintln!("[voice] dsp socket error: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            if n < 12 || buf[0] >> 6 != 2 {
                continue;
            }
            self.rx_dsp.fetch_add(1, Ordering::Relaxed);
            let now = Instant::now();
            let refill = (now.duration_since(last_refill).as_micros() / REFILL_US) as u32;
            if refill > 0 {
                tokens = (tokens + refill).min(MAX_BURST);
                last_refill = now;
            }
            if tokens == 0 {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let cc = (buf[0] & 0x0f) as usize;
            let hdr = 12 + 4 * cc;
            if n <= hdr || n - hdr > 1024 {
                continue;
            }
            if ulaw_level(&buf[hdr..n]) >= LOUD_LEVEL {
                self.dsp_loud.fetch_add(1, Ordering::Relaxed);
            }
            let Some(phone) = *self.phone.lock().unwrap() else {
                continue;
            };
            tokens -= 1;
            out[6..8].copy_from_slice(&buf[2..4]); // RTP seq
            out[8..12].copy_from_slice(&buf[4..8]); // RTP timestamp
            let payload = &buf[hdr..n];
            out[HDR_LEN..HDR_LEN + payload.len()].copy_from_slice(payload);
            if self.phone_sock.send_to(&out[..HDR_LEN + payload.len()], phone).is_ok() {
                self.tx_phone.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// What the DSP sent since a start while ringing: packets, and how many had sound.
    fn early_report(&self, rx_at: u64, loud_at: u64) -> String {
        let rx = self.rx_dsp.load(Ordering::Relaxed).saturating_sub(rx_at);
        let loud = self.dsp_loud.load(Ordering::Relaxed).saturating_sub(loud_at);
        format!("early media: {rx} packets from the dsp, {loud} with sound")
    }

    /// Owns the ODU process: starts it once the call is active, restarts it if it dies
    /// during the call, kills it when the call is gone, ends the session when idle. While an
    /// outgoing call rings it tries the tool for early media (see the module docs).
    fn monitor(&self, at_port: Arc<AtPort>, launch: OduLaunch) {
        let mut child: Option<Child> = None;
        let mut idle_since: Option<Instant> = None;
        let mut last_spawn: Option<Instant> = None;
        let mut spawns_this_call: u32 = 0;
        let mut active_since: Option<Instant> = None;
        let mut rx_at_spawn: u64 = 0;
        let mut attached = false;
        // The running child was started while the call was still ringing.
        let mut early_child = false;
        let mut early_tries: u32 = 0;
        let mut loud_at_spawn: u64 = 0;
        let mut dialing_since: Option<Instant> = None;
        // A reply can lose its +CLCC line to another AT user under load; one empty reading
        // is not enough to tear the voice path down.
        let mut idle_readings: u32 = 0;
        while !self.stop.load(Ordering::Relaxed) && self.started.elapsed() < MAX_SESSION {
            // Reap a child that died on its own.
            if let Some(c) = child.as_mut() {
                if let Ok(Some(status)) = c.try_wait() {
                    eprintln!("[voice] odu exited: {status}");
                    child = None;
                    self.odu_running.store(false, Ordering::Relaxed);
                }
            }
            let (state, voice_calls, conclusive) = match at_cmd::send(&at_port, "AT+CLCC", 2) {
                Ok(resp) => {
                    let calls: Vec<Value> = parse_clcc(&resp)
                        .into_iter()
                        .filter(|c| c["mode"].as_i64() == Some(0))
                        .collect();
                    let conclusive = !calls.is_empty() || resp.contains("OK");
                    let state = calls
                        .iter()
                        .filter_map(|c| c["stat"].as_str())
                        .find(|s| *s == "active")
                        .or_else(|| calls.first().and_then(|c| c["stat"].as_str()))
                        .unwrap_or("none")
                        .to_string();
                    (state, calls.len(), conclusive)
                }
                Err(e) => {
                    eprintln!("[voice] AT+CLCC failed: {e}");
                    ("unknown".to_string(), 0, false)
                }
            };
            if conclusive {
                *self.call_state.lock().unwrap() = state.clone();
            }
            if conclusive && voice_calls == 0 {
                idle_readings += 1;
                if idle_readings < 2 {
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
                if let Some(c) = child.take() {
                    if early_child {
                        eprintln!("[voice] call ended while ringing; {}", self.early_report(rx_at_spawn, loud_at_spawn));
                    }
                    eprintln!("[voice] call gone, stopping odu");
                    stop_child(c);
                    self.odu_running.store(false, Ordering::Relaxed);
                }
                spawns_this_call = 0;
                active_since = None;
                attached = false;
                early_child = false;
                early_tries = 0;
                dialing_since = None;
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= IDLE_END {
                    eprintln!("[voice] idle, ending session");
                    break;
                }
            } else if conclusive {
                idle_since = None;
                idle_readings = 0;
                if state == "active" {
                    active_since.get_or_insert_with(Instant::now);
                } else if state != "held" {
                    active_since = None;
                }
                if state == "dialing" {
                    dialing_since.get_or_insert_with(Instant::now);
                } else {
                    dialing_since = None;
                }
                let ringing = matches!(state.as_str(), "dialing" | "alerting");
                // Answered (or anything but ringing): the instance started while ringing never
                // carries the call itself; the call gets a fresh start below, as before.
                if early_child && !ringing {
                    if let Some(c) = child.take() {
                        eprintln!("[voice] ringing over ({state}); {}", self.early_report(rx_at_spawn, loud_at_spawn));
                        stop_child(c);
                    }
                    self.odu_running.store(false, Ordering::Relaxed);
                    early_child = false;
                    attached = false;
                }
                // Started but no DSP audio: the tool missed the call, start over.
                if let (Some(spawned), Some(_)) = (last_spawn, child.as_ref()) {
                    let rx = self.rx_dsp.load(Ordering::Relaxed);
                    if !attached && rx.saturating_sub(rx_at_spawn) >= ATTACH_MIN_PACKETS {
                        attached = true;
                        let phase = if early_child { " while ringing (early media)" } else { "" };
                        eprintln!("[voice] dsp audio flowing {} ms after start{phase}", spawned.elapsed().as_millis());
                    } else if !attached && early_child && spawned.elapsed() > ATTACH_TIMEOUT {
                        eprintln!(
                            "[voice] no dsp audio {} ms after a start while ringing; {}",
                            spawned.elapsed().as_millis(),
                            self.early_report(rx_at_spawn, loud_at_spawn)
                        );
                        if let Some(c) = child.take() {
                            stop_child(c);
                        }
                        self.odu_running.store(false, Ordering::Relaxed);
                        early_child = false;
                    } else if !attached && spawned.elapsed() > ATTACH_TIMEOUT && spawns_this_call < 4 {
                        eprintln!("[voice] no dsp audio {} ms after start, restarting odu", spawned.elapsed().as_millis());
                        if let Some(c) = child.take() {
                            stop_child(c);
                        }
                        self.odu_running.store(false, Ordering::Relaxed);
                        active_since = Some(Instant::now());
                    }
                }
                let settled = active_since.map(|t| t.elapsed() >= START_DELAY).unwrap_or(false);
                let backoff_ok = last_spawn.map(|t| t.elapsed() > Duration::from_millis(1500)).unwrap_or(true);
                if state == "active" && settled && child.is_none() && backoff_ok && spawns_this_call < 4 {
                    match spawn_odu(&launch) {
                        Ok(c) => {
                            eprintln!("[voice] odu started (pid {}) for active call (attempt {})", c.id(), spawns_this_call + 1);
                            child = Some(c);
                            last_spawn = Some(Instant::now());
                            rx_at_spawn = self.rx_dsp.load(Ordering::Relaxed);
                            attached = false;
                            spawns_this_call += 1;
                            self.odu_starts.fetch_add(1, Ordering::Relaxed);
                            self.odu_running.store(true, Ordering::Relaxed);
                        }
                        Err(e) => {
                            eprintln!("[voice] odu spawn failed: {e}");
                            last_spawn = Some(Instant::now());
                        }
                    }
                }
                let probe_due = state == "alerting"
                    || dialing_since.map(|t| t.elapsed() >= DIALING_PROBE_AFTER).unwrap_or(false);
                if ringing && probe_due && voice_calls == 1 && child.is_none() && backoff_ok && early_tries < EARLY_TRIES {
                    early_tries += 1;
                    match spawn_odu(&launch) {
                        Ok(c) => {
                            eprintln!("[voice] odu started (pid {}) while {state}, listening for early media (try {early_tries})", c.id());
                            child = Some(c);
                            last_spawn = Some(Instant::now());
                            rx_at_spawn = self.rx_dsp.load(Ordering::Relaxed);
                            loud_at_spawn = self.dsp_loud.load(Ordering::Relaxed);
                            attached = false;
                            early_child = true;
                            self.odu_starts.fetch_add(1, Ordering::Relaxed);
                            self.odu_running.store(true, Ordering::Relaxed);
                        }
                        Err(e) => {
                            eprintln!("[voice] odu spawn failed while ringing: {e}");
                            last_spawn = Some(Instant::now());
                        }
                    }
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        if let Some(c) = child.take() {
            stop_child(c);
        }
        self.odu_running.store(false, Ordering::Relaxed);
        self.ended.store(true, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        if let Some(phone) = *self.phone.lock().unwrap() {
            let mut bye = [0u8; HDR_LEN];
            bye[..4].copy_from_slice(&MAGIC);
            bye[4] = TYPE_BYE;
            bye[12..28].copy_from_slice(&self.key);
            let _ = self.phone_sock.send_to(&bye, phone);
        }
        eprintln!(
            "[voice] session ended after {}s: rx_phone={} tx_phone={} rx_dsp={} dropped={}",
            self.started.elapsed().as_secs(),
            self.rx_phone.load(Ordering::Relaxed),
            self.tx_phone.load(Ordering::Relaxed),
            self.rx_dsp.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed)
        );
    }
}

fn bind_with_retry(phone_port: u16) -> Result<(UdpSocket, UdpSocket), String> {
    let mut last = String::new();
    for _ in 0..15 {
        match (
            UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, phone_port)),
            UdpSocket::bind(SocketAddrV4::new(IDU_ADDR, RTP_PORT)),
        ) {
            (Ok(p), Ok(d)) => return Ok((p, d)),
            (p, d) => {
                last = match (p, d) {
                    (Err(e), _) => format!("bind udp {phone_port}: {e}"),
                    (_, Err(e)) => format!("bind {IDU_ADDR}:{RTP_PORT}: {e}"),
                    _ => unreachable!(),
                };
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(last)
}

fn spawn_odu(launch: &OduLaunch) -> Result<Child, String> {
    let mut cmd = Command::new(ODU_BIN);
    cmd.args(["-i", &launch.vsid, "-d", &launch.devices, "-p", "dummy", "-r", "dummy", "-t", "0"]);
    if launch.mute_tx {
        cmd.args(["-u", "0"]);
    }
    cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    die_with_parent(&mut cmd);
    cmd.spawn().map_err(|e| format!("spawn {ODU_BIN}: {e}"))
}

/// Audio threads run at a higher nice level. Not SCHED_FIFO: when the DSP tool is not attached
/// it emits thousands of silent packets per second, and a real-time thread spinning on those
/// starved the tunnel's own threads until the relay dropped the session mid-call.
pub fn raise_audio_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain libc call on the current thread.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, -8);
    }
}

/// SIGINT lets the tool close the voice graph cleanly; SIGKILL after 2 s.
fn stop_child(mut child: Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: plain signal to a pid we own.
    unsafe { libc::kill(pid, libc::SIGINT) };
    for _ in 0..20 {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_stray_odu() {
    let _ = Command::new("killall")
        .args(["-9", "zte_route_hal_voice_test"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn ensure_lo_addresses() {
    for addr in [ODU_ADDR, IDU_ADDR] {
        let _ = Command::new("ip")
            .args(["addr", "add", &format!("{addr}/24"), "dev", "lo"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(target_os = "linux")]
fn die_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id();
    // SAFETY: async-signal-safe calls only, in the forked child before exec.
    unsafe {
        cmd.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
            if libc::getppid() as u32 != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_cmd: &mut Command) {}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Mean absolute linear level (0..=32124) of a G.711 µ-law payload.
fn ulaw_level(payload: &[u8]) -> u32 {
    if payload.is_empty() {
        return 0;
    }
    let sum: u64 = payload
        .iter()
        .map(|&b| {
            let u = !b;
            let exponent = (u >> 4) & 0x07;
            let mantissa = (u & 0x0f) as u64;
            (((mantissa << 3) + 0x84) << exponent) - 0x84
        })
        .sum();
    (sum / payload.len() as u64) as u32
}

#[cfg(test)]
mod tests {
    #[test]
    fn ulaw_level_tells_sound_from_silence() {
        assert_eq!(ulaw_level(&[0xFF; 160]), 0, "positive zero");
        assert_eq!(ulaw_level(&[0x7F; 160]), 0, "negative zero");
        assert_eq!(ulaw_level(&[0x00; 160]), 32124, "full scale");
        assert!(ulaw_level(&[0xFE; 160]) < LOUD_LEVEL, "comfort noise");
        assert!(ulaw_level(&[0x9F, 0x1F].repeat(80)) >= LOUD_LEVEL, "a tone at about -12 dBFS");
        assert_eq!(ulaw_level(&[]), 0);
    }

    use super::*;

    #[test]
    fn header_layout() {
        assert_eq!(HDR_LEN, 4 + 1 + 1 + 2 + 4 + 16);
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
    }
}
