//! OMA MMS encapsulation (MM1) codec: the subset a phone needs. WSP binary headers,
//! m-notification.ind / m-retrieve.conf / m-send.conf decoding, m-send.req and
//! m-notifyresp.ind encoding. Reference: OMA-WAP-MMS-ENC and WAP-230-WSP.

use std::collections::BTreeMap;

pub const MSG_SEND_REQ: u8 = 0x80;
#[allow(dead_code)]
pub const MSG_SEND_CONF: u8 = 0x81;
pub const MSG_NOTIFICATION_IND: u8 = 0x82;
pub const MSG_NOTIFYRESP_IND: u8 = 0x83;
pub const MSG_RETRIEVE_CONF: u8 = 0x84;
pub const MSG_DELIVERY_IND: u8 = 0x86;

// Header field codes (with the 0x80 bit).
const F_BCC: u8 = 0x81;
const F_CC: u8 = 0x82;
const F_CONTENT_LOCATION: u8 = 0x83;
const F_CONTENT_TYPE: u8 = 0x84;
const F_DATE: u8 = 0x85;
const F_DELIVERY_REPORT: u8 = 0x86;
const F_EXPIRY: u8 = 0x88;
const F_FROM: u8 = 0x89;
const F_MESSAGE_CLASS: u8 = 0x8A;
const F_MESSAGE_ID: u8 = 0x8B;
const F_MESSAGE_TYPE: u8 = 0x8C;
const F_MMS_VERSION: u8 = 0x8D;
const F_MESSAGE_SIZE: u8 = 0x8E;
const F_RESPONSE_STATUS: u8 = 0x92;
const F_RESPONSE_TEXT: u8 = 0x93;
const F_STATUS: u8 = 0x95;
const F_SUBJECT: u8 = 0x96;
const F_TO: u8 = 0x97;
const F_TRANSACTION_ID: u8 = 0x98;

// Part header codes.
const P_CONTENT_LOCATION: u8 = 0x8E;
const P_CONTENT_ID: u8 = 0xC0;

// WSP well-known content types (without the 0x80 bit).
const CT_TEXT_PLAIN: u8 = 0x03;
const CT_IMAGE_GIF: u8 = 0x1D;
const CT_IMAGE_JPEG: u8 = 0x1E;
const CT_IMAGE_PNG: u8 = 0x20;
const CT_MULTIPART_MIXED: u8 = 0x23;
const CT_MULTIPART_RELATED: u8 = 0x33;
const CT_MMS_MESSAGE: u8 = 0x3E;

// ---- primitives ----

pub fn uintvar(mut n: usize) -> Vec<u8> {
    let mut out = vec![(n & 0x7F) as u8];
    n >>= 7;
    while n > 0 {
        out.insert(0, ((n & 0x7F) as u8) | 0x80);
        n >>= 7;
    }
    out
}

pub fn read_uintvar(d: &[u8], i: &mut usize) -> Option<usize> {
    let mut n: usize = 0;
    for _ in 0..5 {
        let b = *d.get(*i)?;
        *i += 1;
        n = (n << 7) | (b & 0x7F) as usize;
        if b & 0x80 == 0 {
            return Some(n);
        }
    }
    None
}

/// WSP Value-length: 0..30 inline, 31 followed by a uintvar.
fn read_value_length(d: &[u8], i: &mut usize) -> Option<usize> {
    let b = *d.get(*i)?;
    if b < 31 {
        *i += 1;
        Some(b as usize)
    } else if b == 31 {
        *i += 1;
        read_uintvar(d, i)
    } else {
        None
    }
}

fn value_length(n: usize) -> Vec<u8> {
    if n < 31 {
        vec![n as u8]
    } else {
        let mut v = vec![31];
        v.extend(uintvar(n));
        v
    }
}

/// Text-string: quoted with 0x7F when the first byte would look like a short integer.
pub fn text(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 2);
    if s.as_bytes().first().map_or(false, |b| *b >= 0x80) {
        v.push(0x7F);
    }
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

fn read_text(d: &[u8], i: &mut usize) -> Option<String> {
    let start = *i;
    let end = d[start..].iter().position(|b| *b == 0)? + start;
    *i = end + 1;
    let mut s = &d[start..end];
    if s.first() == Some(&0x7F) {
        s = &s[1..];
    }
    Some(String::from_utf8_lossy(s).into_owned())
}

/// Skips any WSP header value (short integer, text or value-length prefixed block).
fn skip_value(d: &[u8], i: &mut usize) -> Option<()> {
    let b = *d.get(*i)?;
    if b >= 0x80 {
        *i += 1;
    } else if b >= 32 {
        read_text(d, i)?;
    } else {
        let n = read_value_length(d, i)?;
        *i += n;
    }
    Some(())
}

/// Encoded-string-value / Text-string: either a text or a value-length block with an
/// optional charset byte in front of the text.
fn read_encoded_string(d: &[u8], i: &mut usize) -> Option<String> {
    let b = *d.get(*i)?;
    if b < 32 {
        let n = read_value_length(d, i)?;
        let end = *i + n;
        let block = d.get(*i..end)?;
        *i = end;
        let mut j = 0;
        if block.first().map_or(false, |c| *c >= 0x80) {
            j = 1; // charset short-int; treat as UTF-8 either way
        }
        let mut k = j;
        let s = read_text(block, &mut k).unwrap_or_default();
        Some(s)
    } else {
        read_text(d, i)
    }
}

fn read_long_integer(d: &[u8], i: &mut usize) -> Option<u64> {
    let n = *d.get(*i)? as usize;
    *i += 1;
    let bytes = d.get(*i..*i + n)?;
    *i += n;
    let mut v: u64 = 0;
    for b in bytes {
        v = (v << 8) | *b as u64;
    }
    Some(v)
}

// ---- content types ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentType {
    pub mime: String,
    pub params: BTreeMap<String, String>,
}

fn well_known_mime(code: u8) -> Option<&'static str> {
    Some(match code {
        0x00 => "*/*",
        0x02 => "text/html",
        CT_TEXT_PLAIN => "text/plain",
        0x05 => "text/vnd.wap.wml",
        0x1D => "image/gif",
        0x1E => "image/jpeg",
        0x1F => "image/tiff",
        0x20 => "image/png",
        0x21 => "image/vnd.wap.wbmp",
        0x22 => "application/vnd.wap.multipart.*",
        CT_MULTIPART_MIXED => "application/vnd.wap.multipart.mixed",
        0x26 => "application/vnd.wap.multipart.alternative",
        0x27 => "application/xml",
        0x28 => "text/xml",
        CT_MULTIPART_RELATED => "application/vnd.wap.multipart.related",
        CT_MMS_MESSAGE => "application/vnd.wap.mms-message",
        _ => return None,
    })
}

fn well_known_code(mime: &str) -> Option<u8> {
    Some(match mime {
        "text/plain" => CT_TEXT_PLAIN,
        "image/gif" => CT_IMAGE_GIF,
        "image/jpeg" | "image/jpg" => CT_IMAGE_JPEG,
        "image/png" => CT_IMAGE_PNG,
        "application/vnd.wap.multipart.mixed" => CT_MULTIPART_MIXED,
        "application/vnd.wap.multipart.related" => CT_MULTIPART_RELATED,
        _ => return None,
    })
}

/// Parses a Content-Type value (Content-general-form, constrained media or text).
fn read_content_type(d: &[u8], i: &mut usize) -> Option<ContentType> {
    let b = *d.get(*i)?;
    let mut params = BTreeMap::new();
    if b >= 0x80 {
        *i += 1;
        return Some(ContentType { mime: well_known_mime(b & 0x7F).unwrap_or("application/octet-stream").to_string(), params });
    }
    if b >= 32 {
        let s = read_text(d, i)?;
        return Some(ContentType { mime: s.to_ascii_lowercase(), params });
    }
    let n = read_value_length(d, i)?;
    let end = *i + n;
    let block = d.get(*i..end)?;
    *i = end;
    let mut j = 0;
    let mime = match *block.first()? {
        c if c >= 0x80 => {
            j = 1;
            well_known_mime(c & 0x7F).unwrap_or("application/octet-stream").to_string()
        }
        _ => {
            let s = read_text(block, &mut j)?;
            s.to_ascii_lowercase()
        }
    };
    // Parameters: well-known (short int) or text names, values short int / text.
    while j < block.len() {
        let p = block[j];
        let name: String = if p >= 0x80 {
            j += 1;
            match p {
                0x81 => "charset".into(),
                0x85 => "name".into(),
                0x89 => "type".into(),
                0x8A => "start".into(),
                0x99 => "start".into(),
                0x9E => "type".into(),
                0x97 => "name".into(),
                other => format!("p{:02x}", other),
            }
        } else {
            match read_text(block, &mut j) {
                Some(s) => s.to_ascii_lowercase(),
                None => break,
            }
        };
        let v = match block.get(j) {
            Some(c) if *c >= 0x80 => {
                j += 1;
                match (name.as_str(), *c) {
                    ("charset", 0xEA) => "utf-8".into(),
                    ("charset", 0x83) => "us-ascii".into(),
                    ("charset", 0x84) => "iso-8859-1".into(),
                    (_, c) => format!("{}", c & 0x7F),
                }
            }
            Some(c) if *c >= 32 => read_text(block, &mut j).unwrap_or_default(),
            Some(_) => {
                // value-length block (e.g. integer charset); keep raw
                let n = read_value_length(block, &mut j).unwrap_or(0);
                let raw = block.get(j..j + n).unwrap_or(&[]).to_vec();
                j += n;
                raw.iter().map(|b| format!("{:02x}", b)).collect()
            }
            None => break,
        };
        params.insert(name, v);
    }
    Some(ContentType { mime, params })
}

fn encode_content_type(mime: &str, params: &[(&str, &str)]) -> Vec<u8> {
    let mut body = match well_known_code(mime) {
        Some(c) => vec![c | 0x80],
        None => text(mime),
    };
    for (k, v) in params {
        match (*k, *v) {
            ("charset", "utf-8") => body.extend([0x81, 0xEA]),
            ("type", v) => {
                body.push(0x89);
                body.extend(text(v));
            }
            ("start", v) => {
                body.push(0x8A);
                body.extend(text(v));
            }
            (k, v) => {
                body.extend(text(k));
                body.extend(text(v));
            }
        }
    }
    if params.is_empty() && body.len() == 1 {
        return body;
    }
    let mut out = value_length(body.len());
    out.extend(body);
    out
}

// ---- messages ----

#[derive(Debug, Clone, Default)]
pub struct Headers {
    pub message_type: u8,
    pub transaction_id: String,
    pub version: u8,
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub message_id: String,
    pub content_location: String,
    pub message_size: u64,
    pub expiry_secs: Option<u64>,
    pub date: Option<u64>,
    pub response_status: Option<u8>,
    pub response_text: String,
    pub content_type: Option<ContentType>,
    /// Offset of the body (after Content-Type), when present.
    pub body_at: usize,
}

/// Address without the "/TYPE=PLMN" suffix.
pub fn strip_address(s: &str) -> String {
    s.split("/TYPE=").next().unwrap_or(s).trim().to_string()
}

pub fn decode_headers(d: &[u8]) -> Option<Headers> {
    let mut h = Headers::default();
    let mut i = 0;
    while i < d.len() {
        let f = d[i];
        if f < 0x80 {
            // Text header name: skip name + value.
            read_text(d, &mut i)?;
            skip_value(d, &mut i)?;
            continue;
        }
        i += 1;
        match f {
            F_MESSAGE_TYPE => { h.message_type = *d.get(i)?; i += 1; }
            F_MMS_VERSION => { h.version = *d.get(i)?; i += 1; }
            F_TRANSACTION_ID => h.transaction_id = read_text(d, &mut i)?,
            F_MESSAGE_ID => h.message_id = read_text(d, &mut i)?,
            F_CONTENT_LOCATION => h.content_location = read_text(d, &mut i)?,
            F_RESPONSE_TEXT => h.response_text = read_encoded_string(d, &mut i)?,
            F_SUBJECT => h.subject = read_encoded_string(d, &mut i)?,
            F_TO | F_CC | F_BCC => h.to.push(strip_address(&read_encoded_string(d, &mut i)?)),
            F_FROM => {
                let n = read_value_length(d, &mut i)?;
                let end = i + n;
                let block = d.get(i..end)?;
                i = end;
                if block.first() == Some(&0x80) {
                    let mut j = 1;
                    h.from = strip_address(&read_encoded_string(block, &mut j).unwrap_or_default());
                }
            }
            F_MESSAGE_SIZE => h.message_size = read_long_integer(d, &mut i)?,
            F_DATE => h.date = Some(read_long_integer(d, &mut i)?),
            F_EXPIRY => {
                let n = read_value_length(d, &mut i)?;
                let end = i + n;
                let block = d.get(i..end)?;
                i = end;
                if block.len() >= 2 {
                    let mut j = 1;
                    if let Some(v) = read_long_integer(block, &mut j) {
                        h.expiry_secs = Some(v);
                    }
                }
            }
            F_RESPONSE_STATUS | F_STATUS => { h.response_status = Some(*d.get(i)?); i += 1; }
            F_CONTENT_TYPE => {
                h.content_type = Some(read_content_type(d, &mut i)?);
                h.body_at = i;
                return Some(h);
            }
            F_MESSAGE_CLASS | F_DELIVERY_REPORT | 0x8F | 0x90 | 0x94 | 0x99 | 0x9A | 0x87 | 0x91 => {
                skip_value(d, &mut i)?;
            }
            _ => skip_value(d, &mut i)?,
        }
    }
    h.body_at = d.len();
    Some(h)
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Part {
    pub content_type: ContentType,
    pub name: String,
    pub content_id: String,
    pub data: Vec<u8>,
}

/// Splits a multipart body (nEntries, then headersLen/dataLen/headers/data per part).
pub fn decode_parts(body: &[u8]) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut i = 0;
    let Some(n) = read_uintvar(body, &mut i) else { return parts };
    for _ in 0..n.min(64) {
        let Some(hl) = read_uintvar(body, &mut i) else { break };
        let Some(dl) = read_uintvar(body, &mut i) else { break };
        let Some(hdr) = body.get(i..i + hl) else { break };
        let Some(data) = body.get(i + hl..i + hl + dl) else { break };
        i += hl + dl;
        let mut j = 0;
        let content_type = read_content_type(hdr, &mut j).unwrap_or(ContentType {
            mime: "application/octet-stream".into(),
            params: BTreeMap::new(),
        });
        let mut name = String::new();
        let mut content_id = String::new();
        while j < hdr.len() {
            let f = hdr[j];
            if f < 0x80 {
                let Some(key) = read_text(hdr, &mut j) else { break };
                let Some(val) = read_text(hdr, &mut j) else { break };
                if key.eq_ignore_ascii_case("content-location") { name = val; }
                else if key.eq_ignore_ascii_case("content-id") { content_id = val; }
                continue;
            }
            j += 1;
            match f {
                P_CONTENT_LOCATION => name = read_text(hdr, &mut j).unwrap_or_default(),
                P_CONTENT_ID => content_id = read_text(hdr, &mut j).unwrap_or_default(),
                _ => { if skip_value(hdr, &mut j).is_none() { break; } }
            }
        }
        if name.is_empty() {
            name = content_type.params.get("name").cloned().unwrap_or_default();
        }
        parts.push(Part { content_type, name, content_id: content_id.trim_matches(|c| c == '"' || c == '<' || c == '>').to_string(), data: data.to_vec() });
    }
    parts
}

/// A part to send.
pub struct OutPart<'a> {
    pub mime: &'a str,
    pub name: &'a str,
    pub data: &'a [u8],
}

/// m-send.req with a multipart/mixed body (text and media parts).
pub fn encode_send_req(txid: &str, to: &[String], subject: &str, parts: &[OutPart]) -> Vec<u8> {
    let mut out = vec![F_MESSAGE_TYPE, MSG_SEND_REQ];
    out.push(F_TRANSACTION_ID);
    out.extend(text(txid));
    out.extend([F_MMS_VERSION, 0x90]);
    out.extend([F_FROM, 0x01, 0x81]); // Insert-address-token
    for t in to {
        out.push(F_TO);
        let addr = if t.chars().all(|c| c.is_ascii_digit() || c == '+') { format!("{t}/TYPE=PLMN") } else { t.clone() };
        out.extend(text(&addr));
    }
    if !subject.is_empty() {
        out.push(F_SUBJECT);
        out.extend(text(subject));
    }
    out.push(F_CONTENT_TYPE);
    out.extend(encode_content_type("application/vnd.wap.multipart.mixed", &[]));
    out.extend(uintvar(parts.len()));
    for p in parts {
        let mut hdr = if p.mime == "text/plain" {
            encode_content_type("text/plain", &[("charset", "utf-8")])
        } else {
            encode_content_type(p.mime, &[])
        };
        if !p.name.is_empty() {
            hdr.push(P_CONTENT_LOCATION);
            hdr.extend(text(p.name));
        }
        out.extend(uintvar(hdr.len()));
        out.extend(uintvar(p.data.len()));
        out.extend(hdr);
        out.extend_from_slice(p.data);
    }
    out
}

/// m-notifyresp.ind telling the MMSC the message was retrieved.
pub fn encode_notify_resp(txid: &str) -> Vec<u8> {
    let mut out = vec![F_MESSAGE_TYPE, MSG_NOTIFYRESP_IND, F_TRANSACTION_ID];
    out.extend(text(txid));
    out.extend([F_MMS_VERSION, 0x90, F_STATUS, 0x81]); // Retrieved
    out
}

pub fn decode_text_part(p: &Part) -> String {
    match p.content_type.params.get("charset").map(|s| s.as_str()) {
        Some("iso-8859-1") | Some("us-ascii") => p.data.iter().map(|b| *b as char).collect(),
        _ => String::from_utf8_lossy(&p.data).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uintvar_roundtrip() {
        for n in [0usize, 1, 127, 128, 300, 16383, 16384, 1_000_000] {
            let v = uintvar(n);
            let mut i = 0;
            assert_eq!(read_uintvar(&v, &mut i), Some(n));
            assert_eq!(i, v.len());
        }
    }

    #[test]
    fn decodes_optus_notification() {
        let hex = "8c829854455354544f4b454e303031008d9089178036313431323334353637382f545950453d504c4d4e008a808805810302a2ff8e02049b83687474703a2f2f766d6d7363312e6f707475732e636f6d2e61753a383030322f3f54455354544f4b454e30303100964f70656e55363020736500";
        let d: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let h = decode_headers(&d).unwrap();
        assert_eq!(h.message_type, MSG_NOTIFICATION_IND);
        assert_eq!(h.transaction_id, "TESTTOKEN001");
        assert_eq!(h.from, "61412345678");
        assert_eq!(h.message_size, 1179);
        assert_eq!(h.expiry_secs, Some(172799));
        assert_eq!(h.content_location, "http://vmmsc1.optus.com.au:8002/?TESTTOKEN001");
        assert_eq!(h.subject, "OpenU60 se");
    }

    #[test]
    fn send_req_roundtrips_through_decoder() {
        let png = b"\x89PNG....";
        let pdu = encode_send_req("t1", &["+61400000000".into()], "Hi", &[
            OutPart { mime: "text/plain", name: "", data: b"hello" },
            OutPart { mime: "image/png", name: "pic.png", data: png },
        ]);
        let h = decode_headers(&pdu).unwrap();
        assert_eq!(h.message_type, MSG_SEND_REQ);
        assert_eq!(h.to, vec!["+61400000000".to_string()]);
        assert_eq!(h.subject, "Hi");
        assert_eq!(h.content_type.as_ref().unwrap().mime, "application/vnd.wap.multipart.mixed");
        let parts = decode_parts(&pdu[h.body_at..]);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].content_type.mime, "text/plain");
        assert_eq!(parts[0].content_type.params.get("charset").map(|s| s.as_str()), Some("utf-8"));
        assert_eq!(parts[0].data, b"hello");
        assert_eq!(parts[1].content_type.mime, "image/png");
        assert_eq!(parts[1].name, "pic.png");
        assert_eq!(parts[1].data, png);
    }

    #[test]
    fn notify_resp_is_well_formed() {
        let v = encode_notify_resp("abc");
        assert_eq!(v, b"\x8c\x83\x98abc\x00\x8d\x90\x95\x81");
    }
}
