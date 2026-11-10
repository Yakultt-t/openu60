#!/usr/bin/env python3
"""Decode QMI raw SMS-DELIVER PDUs (with SMSC prefix), reassemble concatenated WAP Push and print the MMS notification."""
import sys

def semi_octets(b):
    s = "".join(f"{x & 0xF}{x >> 4}" for x in b)
    return s.rstrip("f").replace("f", "")

def parse_deliver(pdu):
    i = 0
    smsc_len = pdu[0]; i = 1 + smsc_len
    first = pdu[i]; i += 1
    udhi = bool(first & 0x40)
    oa_len = pdu[i]; oa_type = pdu[i + 1]; i += 2
    oa_bytes = (oa_len + 1) // 2
    oa = semi_octets(pdu[i:i + oa_bytes]); i += oa_bytes
    if oa_type & 0x70 == 0x10: oa = "+" + oa
    pid = pdu[i]; dcs = pdu[i + 1]; i += 2
    scts = pdu[i:i + 7]; i += 7
    udl = pdu[i]; i += 1
    ud = pdu[i:]
    udh = {}
    body = ud
    if udhi:
        udhl = ud[0]; h = ud[1:1 + udhl]; body = ud[1 + udhl:]
        j = 0
        while j < len(h):
            t, l = h[j], h[j + 1]; v = h[j + 2:j + 2 + l]; j += 2 + l
            if t == 0x00: udh["concat"] = (v[0], v[1], v[2])
            elif t == 0x05: udh["ports"] = (int.from_bytes(v[0:2], "big"), int.from_bytes(v[2:4], "big"))
            else: udh[f"ie{t:02x}"] = v.hex()
    return {"oa": oa, "pid": pid, "dcs": dcs, "udh": udh, "body": body}

def read_value_len(d, i):
    b = d[i]
    if b < 31: return b, i + 1
    if b == 31:
        n = 0; i += 1
        while True:
            n = (n << 7) | (d[i] & 0x7F); i += 1
            if not d[i - 1] & 0x80: return n, i
    raise ValueError("not a value-length")

def read_text(d, i):
    k = d.index(b"\x00", i); s = d[i:k]
    if s[:1] == b"\x7f": s = s[1:]
    return s.decode("utf-8", "replace"), k + 1

MMS_FIELDS = {0x8c: "Message-Type", 0x98: "Transaction-ID", 0x8d: "MMS-Version", 0x89: "From", 0x96: "Subject",
              0x8a: "Message-Class", 0x8e: "Message-Size", 0x88: "Expiry", 0x83: "Content-Location", 0x97: "To",
              0x85: "Date", 0x84: "Content-Type", 0x8b: "Message-ID", 0x92: "Response-Status", 0x95: "Status",
              0x86: "Delivery-Report", 0x8f: "Priority", 0x90: "Read-Reply", 0x94: "Sender-Visibility"}

def decode_mms_headers(d):
    i = 0; out = {}
    while i < len(d):
        f = d[i]; i += 1
        if f < 0x80: break
        name = MMS_FIELDS.get(f, f"0x{f:02x}")
        b = d[i]
        if f == 0x84:  # Content-Type: parse and stop (body follows)
            if b < 32:
                n, j = read_value_len(d, i); ct = d[j:j + n]; i = j + n
                out["Content-Type"] = ct.hex(); return out, i
            elif b < 128:
                s, i = read_text(d, i); out["Content-Type"] = s; return out, i
            else:
                out["Content-Type"] = f"short 0x{b:02x}"; return out, i + 1
        if b < 32:
            n, j = read_value_len(d, i); v = d[j:j + n]; i = j + n
            if f == 0x89 and v[:1] == b"\x80": out[name] = read_text(v, 1)[0]
            elif f == 0x89: out[name] = "insert-address-token"
            elif f == 0x88: out[name] = f"{'absolute' if v[0]==0x80 else 'relative'} {int.from_bytes(v[2:2+v[1]],'big')}s"
            elif f == 0x8e: out[name] = int.from_bytes(v, "big")
            else: out[name] = v.hex()
        elif b < 128:
            s, i = read_text(d, i); out[name] = s
        else:
            out[name] = f"0x{b:02x}"; i += 1
    return out, i

if __name__ == "__main__":
    parts = []
    for hx in sys.argv[1:]:
        p = parse_deliver(bytes.fromhex(hx)); parts.append(p)
        print(f"segment: from={p['oa']} pid={p['pid']} dcs=0x{p['dcs']:02x} udh={p['udh']} body_len={len(p['body'])}")
    parts.sort(key=lambda p: p["udh"].get("concat", (0, 1, 1))[2])
    data = b"".join(p["body"] for p in parts)
    print("WSP push:", data[:8].hex(), "...")
    tid, ptype = data[0], data[1]
    hl, i = read_value_len(data, 2)
    hdrs = data[i:i + hl]; body = data[i + hl:]
    print(f"  transaction={tid} pdu_type=0x{ptype:02x} ({'Push' if ptype==6 else '?'}) headers={hdrs.hex()}")
    print("MMS notification:")
    h, _ = decode_mms_headers(body)
    for k, v in h.items(): print(f"  {k}: {v}")
