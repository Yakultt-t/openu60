# 通话音频

U60 Pro 没有 SLIC 电话口，也没有接出来的模拟音频接口。OpenU60 借用固件里中兴自带的 “ODU” 语音工具，把基带 DSP 里的通话音频以 RTP 的形式取出来、送进去，再由 agent 转给 iPhone。代码在 `zte-agent/src/voice_bridge.rs`（路由器端）、手机 App 里的音频引擎（手机端，不在本仓库）和 `tunnel/src/voice.rs`（远程时的语音通道）。

## 链路

```
上行（你说话）
iPhone 麦克风 → AVAudioEngine（voiceChat 模式，开启系统回声消除）→ 8 kHz → G.711 µ-law，20 ms 一帧
  → U60A 包 ─┬─ 局域网：UDP 直达路由器 :9091
             └─ 远程：本机 127.0.0.1:9091 → 隧道语音包 → 路由器 :9091
  → voice_bridge 上行节拍器（20 ms 时钟）→ PCMU RTP → 192.168.253.1:48894 → ODU 工具 → DSP → 对方

下行（对方说话）
对方 → DSP → ODU 工具 → PCMU RTP（192.168.253.1:48894 → 192.168.253.100:48894）
  → voice_bridge → U60A 包 → iPhone 抖动缓冲 → 听筒/扬声器
```

**ODU 工具**：固件自带 `/usr/bin/zte_route_hal_voice_test`，这是中兴的 ODU 版本。agent 这样启动它：

```
zte_route_hal_voice_test -i 11C05000 -d 2,2147483652 -p dummy -r dummy -t 0
```

- `-i 11C05000` 是 SUB1 的语音会话 ID（VSID）。
- `-d 2,2147483652` 是 PAL 设备对“扬声器 + 内置麦克风”。这台机器上实测能打开的就是这一对；固件默认的设备对和 Host-PCM（HPCM）方式都会在 DSP 配置阶段失败（AUX PCM 端点的校准数据无效）。
- `-p dummy -r dummy` 让它把下行音频以 PCMU RTP 从 `192.168.253.1:48894` 发到 `192.168.253.100:48894`，并把收到的 RTP（源地址必须是 `.100`）播进上行。这两个地址是写死在工具里的，agent 会把它们都加到 `lo` 上，自己守在 `.100`。
- 工具**必须在通话接通之后才启动**：提前启动的话它永远拿不到 DSP 数据，只会不停地吐静音包。所以 agent 每 400 ms 查一次 `AT+CLCC`，看到第一个 `active` 后再等 1.2 秒才启动它；3.5 秒内收不到 DSP 数据（少于 25 个包）就重启，每通电话最多启动 4 次。通话结束 8 秒后关掉会话，单个会话最长 4 小时。

**手机端协议**（UDP，双向相同）：

```
"U60A" | type u8 | flags u8 | seq u16 BE | ts u32 BE | key[16] | payload
type 0 = 20 ms µ-law 音频（160 字节）；1 = hello/保活（路由器原样回送，可用来测往返）；2 = bye
```

会话密钥由 `POST /api/call/audio/start` 下发（要先登录），密钥不对的包直接丢弃。路由器从收到的包里学习手机的地址，所以手机在 NAT 后面也没问题。

**谁能开音频**：只有拨出或接听这通电话的那台设备会打开音频。路由器锁定第一个发来音频的端点，另一个端点要等原端点静默 2.5 秒后才能接管，被拒绝的包计入 `foreign`。以前 iPad 一看到“通话中”就自己开音频流，和 iPhone 抢同一个语音桥，声音来回跳，所以加了这条规则。

**远程时**：语音不放在 QUIC 里，而是在同一个 UDP socket 上单独发包（首字节 `0x2C`）。两个方向的 AES-256-GCM 密钥都从 QUIC 会话导出，所以仍然端到端加密。没有拥塞控制也不重传，晚到的直接丢。每个包都带上前 3 帧，连丢 3 包（60 ms）也不会断音。

## 调优参数（当前代码）

| 位置 | 参数 | 值与原因 |
|---|---|---|
| 路由器下行 | 令牌桶 | 容量 100、每 5 ms 补 1 个（上限约 200 包/秒）。只用来压住工具空转时每秒几千个的静音包；DSP 交出来的音频是一阵一阵的，真实的突发要全部放行。早期容量只有 6，会丢掉约 25% 的音频 |
| 路由器上行 | 节拍器 | 手机来的帧先进缓冲，再按稳定的 20 ms 时钟逐帧发给 DSP。DSP 工具自己没有抖动缓冲，帧到得不均匀，对方听起来就是断断续续的 |
| | 起始/最小/最大深度 | 40 / 40 / 120 ms |
| | 缺帧 | 补一帧静音，目标加深 20 ms；连续 10 帧（200 ms）没数据才重新攒 |
| | 收缩 | 8 秒没缺帧就减 20 ms |
| | 积压 | 超过目标 40 ms 的部分直接丢，防止越聊延迟越大 |
| | 手机静默 | 超过 2 秒收不到手机音频，就一直送静音帧。RTP 流一停，语音通路会落回路由器自己的麦克风，对方就能听到路由器周围的声音 |
| 线程优先级 | nice | 音频线程 nice -8，隧道线程 nice -5。**不要用 SCHED_FIFO**：工具空转时实时线程会把隧道线程饿死，中继会话在通话中途过期 |
| iPhone | 抖动缓冲 | 局域网起始 80 ms、下限 60 ms；隧道起始 160 ms、下限 100 ms；目标上限 300 ms，硬上限为目标加 300 ms |
| | 缺帧 | 只补缺的样本，不整段重新缓冲（以前一次缺帧会空出 120–400 ms，听起来就是“割裂”），目标加 20 ms；15 秒没出问题就减 10 ms |
| | 时钟漂移 | 积压持续高于目标 80 ms 约 1 秒，就跳过 20 ms |
| | 丢包补偿 | 按 U60A 序号判断，丢了 2–4 帧时用上一帧逐帧衰减（×0.6）填补 |
| | 采集 | 20 ms 一片（原先 16 kHz 下一次回调是 64 ms） |
| 通话状态 | 长轮询 | `GET /api/call/status?wait=…&since=…`，状态一变就返回，不再盲目高频轮询 |

## 麦克风模式与静音

基带不支持 `AT+CMUT`（返回 `ERROR`），所以 app 里的静音只是让手机改发静音帧，按钮会立刻生效。问题在于 ODU 工具打开的设备对里包含**路由器自己的麦克风**，对方可能听得到路由器周围的声音。

“设置 → 设备 → 通话音频 → 路由器麦克风”里有三个选项，对应 `PUT /api/call/audio/settings {"mic_mode": …}`，保存在 `/data/openu60/voice.json`，下一通电话生效：

| 选项 | `mic_mode` | 启动参数 |
|---|---|---|
| 打开（默认） | `default` | `-d 2,2147483652` |
| 在调制解调器端静音 | `mute_tx` | 加 `-u 0` |
| 不打开 | `no_mic` | 只开 `-d 2` |

**实验性**：哪种模式既能让对方正常听到 app 传过去的声音，又能让静音彻底静音，还没有定论。请打一通测试电话，分别验证这两件事。

## 已知问题与限制

- 只有 8 kHz 窄带（G.711），音质接近普通电话，不是 VoLTE 高清语音。
- 路由器的内置麦克风可能漏音（见上文）。
- 同一时间只有一台设备拿得到音频，其他设备不能“旁听”或中途加入。
- 远程通话的延迟约等于手机到路由器的往返时间加上抖动缓冲。家里的上游网络如果就是这台 U60 本身，打洞会失败，只能走中继（见 [remote-access.md](remote-access.md)）。
- 通话录音由 app 在手机上完成：把麦克风和对方的声音混成一个 8 kHz 单声道 WAV，存在 app 的 `Documents/Recordings/`。录音前请确认当地法律是否要求对方同意。
- Mac 上 iOS 模拟器里开着的 OpenU60 也会通过轮询看到来电。现在的版本已经不在模拟器里上报 CallKit 来电，但测试时仍建议关掉模拟器里的 app（见 [faq.md](faq.md)）。
- 环境变量 `ZTE_VOICE_PORT`（默认 9091）、`ZTE_VOICE_VSID`（默认 `11C05000`）、`ZTE_VOICE_DEVICES`（默认 `2,2147483652`）可以覆盖默认值，供实验使用。

## 诊断

- **通话界面**：计时器下面有一行诊断，`↓收到 ↑发出 lost 补偿帧 drop 欠载 buf 当前/目标 ms`，后面是线路和 RTT。
- **路由器状态**：通话中或刚挂断时执行 `api GET /api/call/audio/status | jq .`（`api` 函数见 [install.md](install.md) 第 5 节）。重点看 `odu_running`、`odu_starts`、`rx_dsp`（DSP 送来的包）、`rx_phone`/`tx_phone`、`dropped`、`bad_key`、`foreign`、`uplink_depth_ms`/`uplink_target_ms`、`uplink_underruns`、`uplink_dropped`、`uplink_silence`、`mic_mode`。DSP 开始出声一般在启动后约 850 ms。
- **路由器日志**：`adb shell 'grep -a "\[voice\]\|\[call\]" /tmp/zte-agent.log | tail -40'`。每次接听、挂断、拨号、开音频都会记下请求来源（`[call] <动作> from <地址>`），通话列表变化时也有 `[call] clcc voice=[…]`。
- **app 日志**：开发构建会把音频相关事件写到 app 沙盒的 `Documents/openu60-audio.log`（不含号码和音频），可以这样取出：

```bash
xcrun devicectl device copy from --device <UDID> --domain-type appDataContainer \
  --domain-identifier <你的 bundle ID> --source Documents/openu60-audio.log --destination ./openu60-audio.log
```

- **测链路**：走隧道时，“随时随地访问”页面里的“测试通话音频链路”会在 5 秒内给出这条音频路径的往返时间、抖动和丢包。命令行版本：`cargo run -p openu60-tunnel --example phone -- <device_id> <device_key> --probe`。

## 测试脚本 08–11

这些脚本会**真的拨打电话**（可能产生费用），需要 ADB，从 agent 的 API 登录（密码取自 `$U60_HOME/agent-credentials.txt`），结果放在 `$U60_HOME/call_audio_results/`。08–10 是排查“通话没声音”时留下的实验工具，依赖当时手工放到路由器上的辅助脚本（`play_beeps_run.sh`、`voice_probe_run.sh`、`odu_probe_run.sh`、`tcpdump_run.sh`、`rtp_send_run.sh` 等），这些辅助脚本没有收进仓库，只能当作参考。11 可以直接使用。

| 脚本 | 做什么 |
|---|---|
| `08_call_audio_watch.sh` | 等到出现语音通话，用 `hal_rec_test` 录下行、用 `hal_play_test` 往上行放提示音，然后把录音拉回 Mac。最早的实验，在这台机器上读不到数据 |
| `09_call_audio_test.sh <VSID> <号码> <秒>` | 用指定 VSID 重启探测、拨号、录两个方向、挂断，再逐秒分析 WAV 的音量 |
| `10_call_audio_vsid_sweep.sh <号码> <秒> [VSID:before\|active\|restart …]` | 每项拨一次：在拨号前、接通后或“接通后重启”时启动 ODU 工具，tcpdump 抓两个方向的 RTP，往上行注入 1 kHz 提示音，再用 `scripts/u60/tools/rtp_pcap_analyze.py` 分析。正是这个脚本发现了“必须接通后才启动”这条规律 |
| `11_bridge_test.sh <号码\|none> <秒>` | 在路由器上用 Lua 测试客户端（`device/bridge_client.lua`）模拟手机，走和 app 一样的 U60A 协议：发提示音、录下行，结束后转成 `bridge_<号码>.wav` 并逐秒分析。`none` 表示不拨号，只测会话建立和重新绑定 |

运行 11 之前，先把客户端和一段 8 kHz µ-law 的提示音放到路由器上：

```bash
adb push scripts/u60/device/bridge_client.lua scripts/u60/device/bridge_client_run.sh /data/local/tmp/
python3 - > "${U60_HOME:-$HOME/u60}/beep.ulaw" <<'PY'
import math, sys
def ulaw(x):                              # 16 位 PCM → G.711 µ-law
    sign = 0x80 if x < 0 else 0
    x = min(abs(x), 32635) + 0x84
    exp, mask = 7, 0x4000
    while exp > 0 and not x & mask:
        exp -= 1; mask >>= 1
    return ~(sign | exp << 4 | (x >> (exp + 3)) & 0x0F) & 0xFF
out = bytearray()
for i in range(8000 * 10):                # 10 秒：响半秒、停半秒，1 kHz
    on = (i // 4000) % 2 == 0
    out.append(ulaw(int(8000 * math.sin(2 * math.pi * 1000 * i / 8000)) if on else 0))
sys.stdout.buffer.write(out)
PY
adb push "${U60_HOME:-$HOME/u60}/beep.ulaw" /tmp/beep.ulaw
scripts/u60/11_bridge_test.sh none 10        # 先不拨号测一遍
```

`device/audio_probe.sh start|stop|status` 是更早的探测脚本：用同一个工具录两个方向并往上行放提示音。它要求先放好提示音 WAV，同样只当参考。
