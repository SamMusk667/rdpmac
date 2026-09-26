# 声音（M4 第 3 项）

把 Mac 正在播放的声音传到客户端（MS-RDPEA，静态通道 RDPSND）。2026-09-25 实现，mstsc 实测能听到声音；随后发现
客户端越放越晚（1–2 秒、5–6 秒、12 秒），2026-09-26 查明原因并改了采样率、发送节奏和最小化处理，见"延迟"一节。

## 采集

libscreenio 新增 `AudioCapture`（C 接口 `sio_audio_*`，C ABI 1.2），经 ScreenCaptureKit 采集，需要 macOS 13 和屏幕录制
权限，rdpmacd 本来就有这个权限。

- 它是一条只取声音的采集流，独立于画面采集：画面采集在改尺寸、换显示器时会重开，声音不跟着断。ScreenCaptureKit 的
  采集流必须带画面，这条流请求 2x2、每秒一帧的画面并丢弃。
- 采到的是 Mac 上所有 App 播放的声音，rdpmacd 自己的除外。
- ScreenCaptureKit 给的是 32 位浮点样本，通常每个声道一块；库把它交织成 16 位样本，排队等调用方读。来不及读的超过
  约一秒，从最旧的丢起。
- ScreenCaptureKit 只支持 8000、16000、24000 和 48000 Hz（Apple 文档：给别的值就悄悄按 48000 采）。所以 44100 Hz 由
  libscreenio 用 AudioToolbox 的 AudioConverter 从 48000 的采集重采样得到，每块 960 帧进、约 882 帧出，转换器留几帧
  作滤波，下一块补回。
- 没有声音在播放时，采集流不产出数据，读取超时，客户端也就什么都收不到。
- 库记下声音实际的采样率（`source_rate`，重采样的按比例折算）。和客户端播放用的采样率不一致时日志警告
  `the Mac's sound comes at another rate than the client plays it at`：那样客户端会放快或放慢。但这个值来自
  ScreenCaptureKit 的格式描述；可靠的核对是数帧：日志每 10 秒一行 `waves=500`、每块 882 帧就是 44100 帧/秒。

## 传输

IronRDP 0.13 自带 RDPSND 服务端：每个连接向 `SoundFactory` 要一个处理器，处理器列出格式，客户端回复它支持的格式后，
由处理器挑一个并开始发送。

- rdpmac 提供 16 位立体声 PCM，44100 Hz 和 48000 Hz，按这个顺序选客户端也支持的第一个（mstsc 两个都支持，所以
  是 44100）。为什么 44100 优先，见"延迟"。
- 选定后开一个线程，按选定的采样率采集，每块作为一个 Wave2 发出，约 20 ms。连接关闭时线程停止，采集流随之关闭。
- 没有屏幕录制权限时，日志警告一次，这个连接没有声音；采集流打开失败的其他原因每隔两秒重试。第一次采到声音时日志写
  `the Mac's sound is captured and sent`，带上每块的帧数。
- IronRDP 每轮分发事件最多留 4 块声音，积压时丢旧的。服务端这边不会攒出秒级延迟。
- PCM 44.1 kHz 立体声约 1.4 Mbit/s。AAC 这样的压缩格式以后再做。

## 延迟：为什么越放越晚，怎么防（2026-09-26）

mstsc 播放比 Mac 晚 1–2 秒，后来一次晚 5–6 秒，再一次盯着 YouTube 的时间戳看是 12 秒，而且随时间增长。日志显示服务端
严格按实时发送（每 10 秒 500 块，每块 960 帧，即 48000 帧/秒）、每块发出约 1 ms 就被确认，服务端和 libscreenio 的队列
都有上限，凑不出 12 秒——积压在 mstsc 里面。

原因有三层，参考 [macrdp](https://github.com/bitworker20/macrdp)（另一个 macOS RDP 服务端，踩过同样的坑）的记录：

1. **mstsc 播 48 kHz PCM 比实时慢。** macrdp 实测 48 kHz 喂 mstsc"看一分钟 YouTube 尾巴 8–9 秒"，改成对外声明
   44.1 kHz、服务端自己重采样后尾巴几乎为零，并把这个修法一直保留下来；机制他们也没查清，只确认修法有效。我们的
   三次观察（1–2、5–6、12 秒）和"每秒多积 8.8%"（48000/44100 − 1）吻合。所以现在默认 44100 Hz 优先。
2. **mstsc 的播放队列没有上限，也从不追赶**（无界的 waveOut 队列，只按顺序放；有人为此给 mstsc.exe 打过补丁）。
   所以任何一次超发或停顿后补发都变成永久延迟。而且规范（MS-RDPEA 3.2.5.2.1.6）规定客户端"消费"了一块就发
   Wave Confirm，消费包括处理、取消和丢弃——确认从设计上就不反映播放进度，2026-09-26 的实测也是每块发出约 1 ms
   就确认、`held_ms` 为 0。原来靠确认延迟追赶的逻辑因此不可能触发，已经换掉。
3. **最小化时 mstsc 不消费声音**，服务端照发，恢复后积压的都晚放。焦点切换不发任何 PDU，最小化才发 Suppress Output。

现在的做法（`sound.rs`）：

- **账本。** 每条流记下开始时刻和已发送的声音时长，每块发送前比较：已发 + 本块 > 已过时间 + 200 ms 就跳过这一块，
  客户端最多领先实时 200 ms。若已过时间比已发多出 300 ms 以上（没声音在放，或线程停顿过），账本从当前时刻重记，
  之后到的声音不能当作填补这段空档发出去。
- **陈旧的不发。** libscreenio 给每块带上它在 Mac 上播放的时刻和读出时已过的时间（`age`）；超过 200 ms 的块不发，
  它只会排在客户端已有的声音后面、永远晚放。线程停顿后队列里积的一秒声音，只有最新的约 200 ms 发出去。
- **客户端要求不输出时不发。** IronRDP 收到 Suppress Output 时置一个标志（画面那边已经用它暂停采集）；标志连续为真
  1 秒后不再发声音（mstsc 负载高时会短暂地发一下，连接时也发），恢复后账本按空档重记，不会一股脑补发。日志：
  `the client asked for no output, as mstsc does while minimised; withholding the sound` /
  `the client wants output again; the sound resumes`。
- **时间戳。** 每块的 `wTimeStamp` 填构建 PDU 的时间（IronRDP 原来一律填 0），`dwAudioTimeStamp` 填 Mac 开机以来的毫秒
  数，与 Windows 服务端一致。RDP 没有音画同步机制（RDPSND 与 EGFX 各自独立），别指望它们修延迟。
- **报告。** 有声音时每 10 秒一行 `sound delay lead_ms=… captured_ms=… captured_max_ms=… confirmed_ms=… confirmed_max_ms=…
  held_ms=… skipped_ms=… waves=…`：`lead_ms` 是已发的声音领先实时多少，`captured_ms` 是从 Mac 上播放到读出的时间，
  `confirmed_ms` 是从发出到客户端确认（mstsc 收到就确认，所以是网络往返），`skipped_ms` 是这 10 秒跳过的声音。

`audio-rate = 44100 | 48000`（`--audio-rate`）换优先的采样率，默认 44100；48000 留作对比。App 里没有这一项，保存设置时
会保留它。

## 客户端播放时 Mac 静音

`mute-mac = true | false`（`--mute-mac false`），默认打开；App 设置里是 "Mute the Mac meanwhile"。

- 声音开始发给客户端时，把 Mac 当前的默认输出设备静音（libscreenio `OutputMute`，Core Audio 的设备静音属性），日志写
  `the Mac's output is muted while the client plays its sound`；连接结束时恢复它原来的设置。原来就静音的，保持静音。
- 每 2 秒看一次默认输出设备是否换了（例如插上耳机），换了就恢复旧的、静音新的。
- 有些 HDMI 输出没有静音开关，这时日志警告一次，Mac 照常出声。
- 只静音输出设备，App 照常播放，ScreenCaptureKit 照常采集（2026-09-26 实测：Mac 无声，客户端照常听到）。
- rdpmacd 若在静音期间崩溃，Mac 会保持静音，需要手动取消。

## 设置

- `audio = true | false`（`--no-audio`），默认打开；App 设置里是 "Play the Mac's sound on the client"。
- `mute-mac`，见上一节；`audio-rate`，见"延迟"一节。

都要重启服务端才生效，因为声音通道在启动时决定。

## 测试

- 单元测试：格式（44100 优先）、浮点转 16 位与交织、测试音调跨块连续、Wave 数据的字节序和毫秒时间戳、连接关闭后停止
  发送；账本：实时到的声音全发，比实时快 11% 的只发到领先 200 ms，停顿一秒后积压的 50 块只发最新的约 10 块，5 秒空档
  后一次到的 30 块只发约 10 块，每 10 秒报告一次并清空统计；不输出的要求持续 1 秒才生效，撤回后立即恢复。
  libscreenio：AudioConverter 把 48 kHz 的 1 kHz 正弦重采样到 44.1 kHz，一秒进一秒出（差几十帧滤波延迟）、音高不变，
  任意大小的块按比例出；只读地查默认输出设备能否静音。
- 回环：`--test-pattern` 时声音来源换成 440 Hz 的音调，不需要权限。FreeRDP 的 `sfreerdp /sound:sys:fake` 连接后应
  协商到 16 位立体声 PCM 44100 Hz，每块 882 帧（3528 字节，20 ms），每 10 秒 500 块。
- 实测（mstsc）：看 YouTube 一分钟后暂停，客户端还响多久就是积压；用 `--test-pattern` 的 440 Hz 音调在 Windows 侧
  测音高，能验证 mstsc 是否按 44.1k 播 48k 的数据（那样会听到约 404 Hz）。

## 已知限制与下一步

- 只有 PCM；AAC 能把带宽降到约 1/10，也少和视频抢同一条 TCP 连接。
- 没有转发客户端的音量调节。
- 账本只能防服务端这边超发和补发；客户端自己放得慢（原因 1）只能靠换采样率避开。如果 44.1 kHz 下 mstsc 仍然越放越晚，
  下一个可试的是 AAC，或 macrdp 那样的"重建采集流"手动重同步。
- mstsc 重连（例如证书提示）期间新旧两个连接的声音线程可能短暂并存，都往同一条事件通道发；macrdp 用代数计数器只留
  最新的。日志里每 10 秒的 `waves` 若是 1000 而不是 500 就是这种情况；目前没遇到。
