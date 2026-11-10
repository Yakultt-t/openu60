#!/usr/bin/env python3
"""Build a minimal OMA MMS m-send.req (MMS 1.0 encapsulation) with a text part and a PNG."""
import struct, sys, time, zlib

def uintvar(n):
    out = [n & 0x7F]; n >>= 7
    while n:
        out.insert(0, (n & 0x7F) | 0x80); n >>= 7
    return bytes(out)

def text(s):  # Text-string / Encoded-string (null terminated)
    b = s.encode("utf-8")
    if b and b[0] >= 0x80: b = b"\x7f" + b
    return b + b"\x00"

def tiny_png(w=48, h=48, rgb=(0x5C, 0x9E, 0xFF)):
    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))
    def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))

def build(to, subject, body_text, txid):
    hdr = b""
    hdr += b"\x8c\x80"                      # X-Mms-Message-Type: m-send-req
    hdr += b"\x98" + text(txid)             # X-Mms-Transaction-ID
    hdr += b"\x8d\x90"                      # X-Mms-MMS-Version: 1.0
    hdr += b"\x89\x01\x81"                  # From: Insert-address-token
    hdr += b"\x97" + text(f"{to}/TYPE=PLMN")  # To
    hdr += b"\x96" + text(subject)          # Subject
    hdr += b"\x84\xa3"                      # Content-Type: application/vnd.wap.multipart.mixed
    # parts
    png = tiny_png()
    txt = body_text.encode("utf-8")
    parts = []
    # text/plain; charset=utf-8  -> Content-Type value: value-length, 0x83, 0x81 (charset), 0xEA (utf-8)
    ct_text = b"\x03\x83\x81\xea"
    parts.append((ct_text, txt))
    ct_png = b"\xa0"                         # image/png
    ct_png += b"\x8e" + text("pic.png")      # Content-Location
    parts.append((ct_png, png))
    body = uintvar(len(parts))
    for ct, data in parts:
        body += uintvar(len(ct)) + uintvar(len(data)) + ct + data
    return hdr + body

if __name__ == "__main__":
    to, subject, body_text, out = sys.argv[1:5]
    pdu = build(to, subject, body_text, f"openu60-{int(time.time())}")
    open(out, "wb").write(pdu)
    print(f"wrote {out}: {len(pdu)} bytes")
