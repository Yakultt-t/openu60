#!/usr/bin/env python3
"""Analyse a pcap of ZTE ODU RTP (PCMU) traffic: packet pacing, RTP seq/timestamp sanity, decoded audio
level per second for each direction. usage: rtp_pcap_analyze.py file.pcap [out.wav]"""
import sys, struct, math, wave
ULAW = [0]*256
for i in range(256):
    u = ~i & 0xFF; sign = u & 0x80; exp = (u >> 4) & 7; mant = u & 0x0F
    s = ((mant << 3) + 0x84) << exp; s -= 0x84
    ULAW[i] = -s if sign else s
def packets(path):
    with open(path,'rb') as f: d=f.read()
    magic = struct.unpack('<I', d[:4])[0]
    if magic == 0xa1b2c3d4: end='<'
    elif magic == 0xd4c3b2a1: end='>'
    else: raise SystemExit(f"not a classic pcap (magic {magic:#x})")
    link = struct.unpack(end+'I', d[20:24])[0]
    off = 24
    while off + 16 <= len(d):
        ts_s, ts_us, incl, orig = struct.unpack(end+'IIII', d[off:off+16]); off += 16
        pkt = d[off:off+incl]; off += incl
        yield ts_s + ts_us/1e6, link, pkt
def parse(pkt, link):
    if link == 1: ip = pkt[14:]            # ethernet
    elif link == 113: ip = pkt[16:]        # linux cooked
    elif link == 0: ip = pkt[4:]           # null/loopback
    else: return None
    if len(ip) < 20 or ip[0]>>4 != 4 or ip[9] != 17: return None
    ihl = (ip[0]&15)*4; src = '.'.join(map(str, ip[12:16])); dst = '.'.join(map(str, ip[16:20]))
    udp = ip[ihl:]; sp, dp, ln = struct.unpack('>HHH', udp[:6]); payload = udp[8:ln]
    if len(payload) < 12 or payload[0]>>6 != 2: return None
    pt = payload[1]&0x7f; seq, ts = struct.unpack('>HI', payload[2:8])
    return (f"{src}:{sp}->{dst}:{dp}", pt, seq, ts, payload[12:])
flows = {}
for t, link, pkt in packets(sys.argv[1]):
    r = parse(pkt, link)
    if not r: continue
    key, pt, seq, ts, pl = r
    flows.setdefault(key, []).append((t, pt, seq, ts, pl))
for key, L in flows.items():
    t0 = L[0][0]; dur = L[-1][0]-t0
    print(f"\n== flow {key}: {len(L)} pkts over {dur:.1f}s, pt={L[0][1]}, payload {len(L[0][4])} B")
    # pacing: packets per second buckets
    per = {}
    for t,_,_,_,_ in L: per[int(t-t0)] = per.get(int(t-t0),0)+1
    seqs = [s for _,_,s,_,_ in L]; dseq = [ (seqs[i+1]-seqs[i])&0xffff for i in range(len(seqs)-1)]
    print("   seq deltas (first 10):", dseq[:10], " ts deltas (first 5):", [ (L[i+1][3]-L[i][3])&0xffffffff for i in range(min(5,len(L)-1))])
    pcm = []
    print("   sec  pkts   rms   peak  nonzero%")
    for sec in sorted(per):
        chunk = [p for p in L if int(p[0]-t0)==sec]
        samples = [ULAW[b] for p in chunk for b in p[4]]
        rms = math.sqrt(sum(x*x for x in samples)/max(1,len(samples))); peak = max((abs(x) for x in samples), default=0)
        nz = 100*sum(1 for x in samples if abs(x)>8)/max(1,len(samples))
        print(f"   {sec:3d} {per[sec]:6d} {rms:6.0f} {peak:6d} {nz:6.1f}")
        if sec < 60: pcm.extend(samples[:8000*2])   # keep <=2 s per second (spinning sends dups)
    if len(sys.argv) > 2 and pcm:
        name = sys.argv[2].replace('.wav', '_' + key.replace(':','_').replace('->','_to_') + '.wav')
        w = wave.open(name,'wb'); w.setnchannels(1); w.setsampwidth(2); w.setframerate(8000)
        w.writeframes(struct.pack('<%dh'%len(pcm), *[max(-32768,min(32767,x)) for x in pcm])); w.close(); print("   wrote", name)
