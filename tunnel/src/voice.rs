//! Call audio travels outside QUIC. QUIC datagrams share the connection's congestion window
//! and pacing with the HTTP streams, so one lossy stretch on the router's 5G link throttles
//! the audio behind retransmissions and ACK clocks. These packets are plain AEAD-sealed UDP on
//! the same socket (direct or via the relay's DATA frames), keyed from the QUIC session, so
//! the tunnel stays end-to-end encrypted while audio behaves like RTP: late is dropped, never
//! queued. Every packet also repeats the three previous frames, which hides loss bursts of up
//! to 60 ms (common on a weak 5G link) at the price of four times the (tiny) audio bandwidth.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};

/// First byte of a voice packet. Like the relay's 0x2A it has the QUIC fixed bit clear.
pub const VOICE_MAGIC: u8 = 0x2C;
pub const FLAG_AUDIO: u8 = 0;
pub const FLAG_ECHO_REQUEST: u8 = 1;
pub const FLAG_ECHO_REPLY: u8 = 2;

const HEADER_LEN: usize = 1 + 8;
const TAG_LEN: usize = 16;
const LABEL_C2S: &[u8] = b"openu60 voice c2s";
const LABEL_S2C: &[u8] = b"openu60 voice s2c";

pub enum Incoming {
    /// Audio frames not seen before, oldest first.
    Audio(Vec<Vec<u8>>),
    EchoRequest(Vec<u8>),
    EchoReply(Vec<u8>),
}

/// How many earlier frames ride along with each new one.
const REDUNDANCY: usize = 3;

#[derive(Default)]
struct TxState {
    next_seq: u32,
    /// Most recent first.
    prev: std::collections::VecDeque<Vec<u8>>,
}

#[derive(Default)]
struct RxState {
    highest: Option<u32>,
    /// Bit i set = frame `highest - i` already delivered.
    seen: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct VoiceStats {
    pub packets_in: u64,
    pub frames_in: u64,
    pub frames_recovered: u64,
    pub frames_dup: u64,
    pub bad_packets: u64,
    pub packets_out: u64,
}

pub struct VoiceChannel {
    seal_key: LessSafeKey,
    open_key: LessSafeKey,
    counter: AtomicU64,
    tx: Mutex<TxState>,
    rx: Mutex<RxState>,
    stats: Mutex<VoiceStats>,
}

impl VoiceChannel {
    /// Derives the two directional keys from the QUIC session; `is_client` picks which is ours.
    pub fn from_connection(conn: &quinn::Connection, is_client: bool) -> Result<Self, String> {
        let mut c2s = [0u8; 32];
        let mut s2c = [0u8; 32];
        conn.export_keying_material(&mut c2s, LABEL_C2S, b"").map_err(|e| format!("export key: {e:?}"))?;
        conn.export_keying_material(&mut s2c, LABEL_S2C, b"").map_err(|e| format!("export key: {e:?}"))?;
        let (tx, rx) = if is_client { (c2s, s2c) } else { (s2c, c2s) };
        Self::new(&tx, &rx)
    }

    pub fn new(tx_key: &[u8; 32], rx_key: &[u8; 32]) -> Result<Self, String> {
        let seal = UnboundKey::new(&AES_256_GCM, tx_key).map_err(|_| "bad key")?;
        let open = UnboundKey::new(&AES_256_GCM, rx_key).map_err(|_| "bad key")?;
        Ok(Self {
            seal_key: LessSafeKey::new(seal),
            open_key: LessSafeKey::new(open),
            counter: AtomicU64::new(1),
            tx: Mutex::new(TxState::default()),
            rx: Mutex::new(RxState::default()),
            stats: Mutex::new(VoiceStats::default()),
        })
    }

    pub fn stats(&self) -> VoiceStats {
        self.stats.lock().unwrap().clone()
    }

    /// Wraps one audio frame from the local side, together with the ones before it.
    pub fn seal_audio(&self, frame: &[u8]) -> Vec<u8> {
        let (seq, prev) = {
            let mut tx = self.tx.lock().unwrap();
            let seq = tx.next_seq;
            tx.next_seq = tx.next_seq.wrapping_add(1);
            let prev: Vec<Vec<u8>> = tx.prev.iter().take(seq as usize).cloned().collect();
            tx.prev.push_front(frame.to_vec());
            tx.prev.truncate(REDUNDANCY);
            (seq, prev)
        };
        let mut plain = Vec::with_capacity(8 + frame.len() * (1 + prev.len()));
        plain.push(FLAG_AUDIO);
        plain.extend_from_slice(&seq.to_be_bytes());
        plain.push(1 + prev.len() as u8);
        plain.extend_from_slice(&(frame.len() as u16).to_be_bytes());
        plain.extend_from_slice(frame);
        for p in &prev {
            plain.extend_from_slice(&(p.len() as u16).to_be_bytes());
            plain.extend_from_slice(p);
        }
        self.seal(plain)
    }

    pub fn seal_echo(&self, reply: bool, payload: &[u8]) -> Vec<u8> {
        let mut plain = Vec::with_capacity(1 + payload.len());
        plain.push(if reply { FLAG_ECHO_REPLY } else { FLAG_ECHO_REQUEST });
        plain.extend_from_slice(payload);
        self.seal(plain)
    }

    fn seal(&self, mut plain: Vec<u8>) -> Vec<u8> {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut out = Vec::with_capacity(HEADER_LEN + plain.len() + TAG_LEN);
        out.push(VOICE_MAGIC);
        out.extend_from_slice(&counter.to_be_bytes());
        let nonce = Nonce::assume_unique_for_key(nonce_for(counter));
        // The header is authenticated but not encrypted.
        let aad = out.clone();
        self.seal_key
            .seal_in_place_append_tag(nonce, Aad::from(&aad), &mut plain)
            .expect("aead seal");
        out.extend_from_slice(&plain);
        self.stats.lock().unwrap().packets_out += 1;
        out
    }

    /// Authenticates and decrypts a packet from the peer.
    pub fn open(&self, packet: &[u8]) -> Option<Incoming> {
        if packet.len() < HEADER_LEN + TAG_LEN + 1 || packet[0] != VOICE_MAGIC {
            self.stats.lock().unwrap().bad_packets += 1;
            return None;
        }
        let counter = u64::from_be_bytes(packet[1..9].try_into().ok()?);
        let nonce = Nonce::assume_unique_for_key(nonce_for(counter));
        let mut body = packet[HEADER_LEN..].to_vec();
        let plain = match self.open_key.open_in_place(nonce, Aad::from(&packet[..HEADER_LEN]), &mut body) {
            Ok(p) => p,
            Err(_) => {
                self.stats.lock().unwrap().bad_packets += 1;
                return None;
            }
        };
        self.stats.lock().unwrap().packets_in += 1;
        match plain.first()? {
            &FLAG_AUDIO => Some(Incoming::Audio(self.unpack_audio(&plain[1..]))),
            &FLAG_ECHO_REQUEST => Some(Incoming::EchoRequest(plain[1..].to_vec())),
            &FLAG_ECHO_REPLY => Some(Incoming::EchoReply(plain[1..].to_vec())),
            _ => None,
        }
    }

    fn unpack_audio(&self, body: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Some(seq_bytes) = body.get(0..4) else { return out };
        let seq = u32::from_be_bytes(seq_bytes.try_into().unwrap());
        let Some(&count) = body.get(4) else { return out };
        let mut pos = 5;
        let mut frames: Vec<(u32, &[u8])> = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Some(len) = body.get(pos..pos + 2) else { break };
            let len = u16::from_be_bytes(len.try_into().unwrap()) as usize;
            pos += 2;
            let Some(frame) = body.get(pos..pos + len) else { break };
            pos += len;
            frames.push((seq.wrapping_sub(i as u32), frame));
        }
        // Oldest first, so a recovered frame plays before the one it was carried with.
        frames.reverse();
        let mut rx = self.rx.lock().unwrap();
        let mut stats = self.stats.lock().unwrap();
        for (i, (s, frame)) in frames.iter().enumerate() {
            if rx.accept(*s) {
                stats.frames_in += 1;
                // Anything but the newest frame in a packet is a repeat that filled a hole.
                if i + 1 != frames.len() {
                    stats.frames_recovered += 1;
                }
                out.push(frame.to_vec());
            } else {
                stats.frames_dup += 1;
            }
        }
        out
    }
}

impl RxState {
    /// True if `seq` has not been delivered yet (and marks it delivered).
    fn accept(&mut self, seq: u32) -> bool {
        let Some(highest) = self.highest else {
            self.highest = Some(seq);
            self.seen = 1;
            return true;
        };
        let ahead = seq.wrapping_sub(highest);
        if ahead != 0 && ahead < u32::MAX / 2 {
            self.seen = if ahead >= 64 { 0 } else { self.seen << ahead };
            self.seen |= 1;
            self.highest = Some(seq);
            return true;
        }
        let behind = highest.wrapping_sub(seq);
        if behind >= 64 {
            return false;
        }
        let bit = 1u64 << behind;
        if self.seen & bit != 0 {
            return false;
        }
        self.seen |= bit;
        true
    }
}

fn nonce_for(counter: u64) -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

pub fn is_voice(buf: &[u8]) -> bool {
    !buf.is_empty() && buf[0] == VOICE_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (VoiceChannel, VoiceChannel) {
        let a = [1u8; 32];
        let b = [2u8; 32];
        (VoiceChannel::new(&a, &b).unwrap(), VoiceChannel::new(&b, &a).unwrap())
    }

    #[test]
    fn audio_with_recovery_and_dedupe() {
        let (tx, rx) = pair();
        let p0 = tx.seal_audio(b"f0");
        let p1 = tx.seal_audio(b"f1");
        let p2 = tx.seal_audio(b"f2");
        let p3 = tx.seal_audio(b"f3");
        let frames = |p: &[u8]| match rx.open(p) {
            Some(Incoming::Audio(f)) => f,
            _ => panic!("not audio"),
        };
        assert_eq!(frames(&p0), vec![b"f0".to_vec()]);
        // p1 lost; p2 carries f2, f1, f0: the new ones come out oldest first.
        assert_eq!(frames(&p2), vec![b"f1".to_vec(), b"f2".to_vec()]);
        // p1 arrives late: everything in it was delivered already.
        assert_eq!(frames(&p1), Vec::<Vec<u8>>::new());
        assert_eq!(frames(&p3), vec![b"f3".to_vec()]);
        let s = rx.stats();
        assert_eq!((s.frames_in, s.frames_recovered, s.frames_dup), (4, 1, 6));
        // Two consecutive losses are still covered.
        let p4 = tx.seal_audio(b"f4");
        let p5 = tx.seal_audio(b"f5");
        let p6 = tx.seal_audio(b"f6");
        drop((p4, p5));
        assert_eq!(frames(&p6), vec![b"f4".to_vec(), b"f5".to_vec(), b"f6".to_vec()]);
    }

    #[test]
    fn tamper_and_wrong_key_rejected() {
        let (tx, rx) = pair();
        let mut p = tx.seal_audio(b"hello");
        assert!(rx.open(&p).is_some());
        p[12] ^= 1;
        assert!(rx.open(&p).is_none());
        assert!(tx.open(&tx.seal_echo(false, b"x")).is_none());
    }

    #[test]
    fn echo_round_trip() {
        let (tx, rx) = pair();
        match rx.open(&tx.seal_echo(false, b"t=1")) {
            Some(Incoming::EchoRequest(p)) => assert_eq!(p, b"t=1"),
            _ => panic!(),
        }
        match tx.open(&rx.seal_echo(true, b"t=1")) {
            Some(Incoming::EchoReply(p)) => assert_eq!(p, b"t=1"),
            _ => panic!(),
        }
    }
}
