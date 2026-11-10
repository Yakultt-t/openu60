//! WAP Push over SMS: SMS-DELIVER PDU parsing (8-bit data with UDH), concatenation
//! reassembly, and unwrapping the WSP push into the MMS notification it carries.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const WAP_PUSH_PORT: u16 = 2948;

#[derive(Debug, Clone)]
pub struct Deliver {
    pub originator: String,
    pub dcs: u8,
    pub dest_port: Option<u16>,
    pub concat: Option<(u16, u8, u8)>, // reference, total, seq (1-based)
    pub data: Vec<u8>,
}

fn semi_octets(b: &[u8]) -> String {
    let mut s = String::new();
    for x in b {
        for n in [x & 0xF, x >> 4] {
            if n < 10 {
                s.push((b'0' + n) as char);
            }
        }
    }
    s
}

/// Parses a QMI raw-read SMS-DELIVER (GW_PP format, SMSC length prefix included).
pub fn parse_deliver(pdu: &[u8]) -> Option<Deliver> {
    let mut i = 0;
    let smsc_len = *pdu.get(i)? as usize;
    i += 1 + smsc_len;
    let first = *pdu.get(i)?;
    i += 1;
    if first & 0x03 != 0x00 {
        return None; // not SMS-DELIVER
    }
    let udhi = first & 0x40 != 0;
    let oa_len = *pdu.get(i)? as usize;
    let oa_type = *pdu.get(i + 1)?;
    i += 2;
    let oa_bytes = (oa_len + 1) / 2;
    let oa_raw = pdu.get(i..i + oa_bytes)?;
    i += oa_bytes;
    let originator = if oa_type & 0x70 == 0x50 {
        // Alphanumeric sender (GSM 7-bit packed); keep a hex marker, rare for WAP push.
        format!("alpha:{}", oa_raw.iter().map(|b| format!("{:02x}", b)).collect::<String>())
    } else {
        let digits = semi_octets(oa_raw);
        if oa_type & 0x70 == 0x10 { format!("+{digits}") } else { digits }
    };
    let _pid = *pdu.get(i)?;
    let dcs = *pdu.get(i + 1)?;
    i += 2 + 7; // PID, DCS, SCTS
    let _udl = *pdu.get(i)?;
    i += 1;
    let ud = pdu.get(i..)?;
    let mut dest_port = None;
    let mut concat = None;
    let mut body = ud;
    if udhi {
        let udhl = *ud.first()? as usize;
        let h = ud.get(1..1 + udhl)?;
        body = ud.get(1 + udhl..)?;
        let mut j = 0;
        while j + 2 <= h.len() {
            let t = h[j];
            let l = h[j + 1] as usize;
            let v = h.get(j + 2..j + 2 + l)?;
            j += 2 + l;
            match (t, l) {
                (0x00, 3) => concat = Some((v[0] as u16, v[1], v[2])),
                (0x08, 4) => concat = Some((u16::from_be_bytes([v[0], v[1]]), v[2], v[3])),
                (0x04, 2) => dest_port = Some(v[0] as u16),
                (0x05, 4) => dest_port = Some(u16::from_be_bytes([v[0], v[1]])),
                _ => {}
            }
        }
    }
    Some(Deliver { originator, dcs, dest_port, concat, data: body.to_vec() })
}

/// Puts concatenated segments back together; complete messages come out in order.
pub struct Assembler {
    pending: HashMap<(String, u16), (Vec<Option<Vec<u8>>>, Instant)>,
}

impl Assembler {
    pub fn new() -> Self {
        Self { pending: HashMap::new() }
    }

    pub fn push(&mut self, d: &Deliver) -> Option<Vec<u8>> {
        self.pending.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(600));
        let Some((reference, total, seq)) = d.concat else { return Some(d.data.clone()) };
        if total == 0 || seq == 0 || seq > total {
            return Some(d.data.clone());
        }
        let key = (d.originator.clone(), reference);
        let entry = self.pending.entry(key.clone()).or_insert_with(|| (vec![None; total as usize], Instant::now()));
        if entry.0.len() != total as usize {
            entry.0 = vec![None; total as usize];
        }
        entry.0[(seq - 1) as usize] = Some(d.data.clone());
        if entry.0.iter().all(|p| p.is_some()) {
            let (parts, _) = self.pending.remove(&key).unwrap();
            Some(parts.into_iter().flatten().flatten().collect())
        } else {
            None
        }
    }
}

/// Unwraps a connectionless WSP push and returns the MMS message body when the push
/// carries application/vnd.wap.mms-message.
pub fn unwrap_push(data: &[u8]) -> Option<&[u8]> {
    let pdu_type = *data.get(1)?;
    if pdu_type != 0x06 && pdu_type != 0x07 {
        return None;
    }
    let mut i = 2;
    let headers_len = super::pdu::read_uintvar(data, &mut i)?;
    let headers = data.get(i..i + headers_len)?;
    let body = data.get(i + headers_len..)?;
    let is_mms = match headers.first()? {
        0xBE => true,
        b if *b >= 0x80 => false,
        b if *b >= 32 => {
            let end = headers.iter().position(|c| *c == 0).unwrap_or(headers.len());
            String::from_utf8_lossy(&headers[..end]).starts_with("application/vnd.wap.mms-message")
        }
        _ => {
            // value-length form: media type inside
            let mut j = 0;
            let n = super::pdu::read_uintvar(headers, &mut j).unwrap_or(0);
            let _ = n;
            headers.get(j) == Some(&0xBE)
        }
    };
    if is_mms { Some(body) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn reassembles_optus_push() {
        let a = hex("07911614910991f844028099000462115032414244850c08047cba020105040b8423f0010607beaf848df3b4848c829854455354544f4b454e303031008d9089178036313431323334353637382f545950453d504c4d4e008a808805810302a2ff8e02049b83687474703a2f2f766d6d7363312e6f707475732e636f6d2e61753a383030322f3f54455354544f4b454e30303100964f70656e5536");
        let b = hex("07911614910991f844028099000462115032414244120c08047cba020205040b8423f03020736500");
        let da = parse_deliver(&a).unwrap();
        let db = parse_deliver(&b).unwrap();
        assert_eq!(da.originator, "99");
        assert_eq!(da.dcs, 0x04);
        assert_eq!(da.dest_port, Some(WAP_PUSH_PORT));
        assert_eq!(da.concat, Some((0x7cba, 2, 1)));
        let mut asm = Assembler::new();
        assert!(asm.push(&db).is_none());
        let full = asm.push(&da).unwrap();
        let mms = unwrap_push(&full).unwrap();
        let h = super::super::pdu::decode_headers(mms).unwrap();
        assert_eq!(h.transaction_id, "TESTTOKEN001");
        assert_eq!(h.subject, "OpenU60 se");
    }
}
