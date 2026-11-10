#!/usr/bin/env python3
"""Decode an MMS header-only response (m-send.conf / m-notification.ind)."""
import sys
data = open(sys.argv[1], "rb").read()
i = 0
names = {0x8c: "Message-Type", 0x98: "Transaction-ID", 0x8d: "MMS-Version", 0x92: "Response-Status", 0x8b: "Message-ID",
         0x93: "Response-Text", 0x89: "From", 0x97: "To", 0x96: "Subject", 0x83: "Content-Location", 0x8e: "Message-Size", 0x88: "Expiry", 0x8a: "Message-Class"}
def read_text(j):
    k = data.index(b"\x00", j); return data[j:k].decode("utf-8", "replace"), k + 1
print("raw hex:", data[:64].hex())
while i < len(data):
    f = data[i]; i += 1
    name = names.get(f, f"0x{f:02x}")
    if i >= len(data): break
    v = data[i]
    if f in (0x8c, 0x8d, 0x92, 0x8a) and v >= 0x80:
        print(f"{name}: 0x{v:02x}"); i += 1
    elif f == 0x89:
        ln = data[i]; print(f"From: {data[i+1:i+1+ln].hex()}"); i += 1 + ln
    elif v < 0x20 and f == 0x8e:
        ln = v; print(f"{name}: {int.from_bytes(data[i+1:i+1+ln],'big')}"); i += 1 + ln
    elif f == 0x88:
        ln = data[i]; print(f"Expiry: {data[i+1:i+1+ln].hex()}"); i += 1 + ln
    else:
        s, i = read_text(i); print(f"{name}: {s}")
