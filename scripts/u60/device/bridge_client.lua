-- Test client for the agent's call-audio bridge, speaking the phone protocol on the device itself.
-- usage: lua bridge_client.lua <key hex> <seconds> <ulaw file to send> <ulaw file to record> [host] [port]
-- Sends hello, then 20 ms µ-law frames from the file (looped); every downlink audio frame's payload
-- is appended to the record file. Prints counters at the end.
local nixio = require("nixio")
local keyhex, secs, infile, outfile = arg[1], tonumber(arg[2] or "20"), arg[3], arg[4]
local host, port = arg[5] or "127.0.0.1", tonumber(arg[6] or "9091")
local key = keyhex:gsub("%x%x", function(h) return string.char(tonumber(h, 16)) end)
assert(#key == 16, "key must be 32 hex chars")
local f = assert(io.open(infile, "rb")); local data = f:read("*a"); f:close()
local out = assert(io.open(outfile, "wb"))
local sock = assert(nixio.socket("inet", "dgram"))
assert(sock:bind("0.0.0.0", 0))
sock:setblocking(false)
local MAGIC = "U60A"
local function be16(v) return string.char(math.floor(v/256)%256, v%256) end
local function be32(v) return string.char(math.floor(v/16777216)%256, math.floor(v/65536)%256, math.floor(v/256)%256, v%256) end
local function pkt(t, seq, ts, payload) return MAGIC .. string.char(t, 0) .. be16(seq % 65536) .. be32(ts % 4294967296) .. key .. (payload or "") end
local function now() local s, us = nixio.gettimeofday(); return s + us/1e6 end
sock:sendto(pkt(1, 0, 0), host, port)
local start = now(); local n, sent, got, hello, bad, bye, pos, seq, ts = 0, 0, 0, 0, 0, 0, 1, 1, 0
local lastseq
local gaps = 0
while true do
  local t = now()
  if t - start > secs then break end
  local target = start + n * 0.02
  if target > t then local d = target - t; nixio.nanosleep(math.floor(d), math.floor((d % 1) * 1e9)) end
  if pos + 159 > #data then pos = 1 end
  if sock:sendto(pkt(0, seq, ts, data:sub(pos, pos + 159)), host, port) then sent = sent + 1 end
  pos = pos + 160; seq = seq + 1; ts = ts + 160; n = n + 1
  while true do
    local d = sock:recv(2048)
    if not d then break end
    if #d >= 28 and d:sub(1, 4) == MAGIC and d:sub(13, 28) == key then
      local typ = d:byte(5)
      if typ == 0 then
        got = got + 1; out:write(d:sub(29))
        local s = d:byte(7) * 256 + d:byte(8)
        if lastseq and (s - lastseq) % 65536 ~= 1 then gaps = gaps + 1 end
        lastseq = s
      elseif typ == 1 then hello = hello + 1
      elseif typ == 2 then bye = bye + 1 end
    else bad = bad + 1 end
  end
end
sock:sendto(pkt(2, seq, ts), host, port)
out:close()
print(string.format("bridge_client: sent=%d got_audio=%d hello=%d bye=%d bad=%d seq_gaps=%d", sent, got, hello, bye, bad, gaps))
