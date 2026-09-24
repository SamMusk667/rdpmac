# ADR-0001：基于 libscreenio + IronRDP 的 macOS RDP 服务端

| 项 | 内容 |
|---|---|
| 状态 | 已接受（2026-09-23，项目负责人对第 11 节的问题给出答复后生效） |
| 日期 | 2026-09-23，同日接受 |
| 决策范围 | rdpmac 项目的技术路线、架构、仓库与许可、里程碑 |
| 输入 | `~/works/libscreenio` 原型；`~/works/rdp-baseline/baseline/2026-09-23-freerdp-macos.md`；IronRDP 源码核对（2026-09-23） |
| 替代方案 | 见第 9 节：FreeRDP 混合架构；Swift/ObjC 从零实现 |

## 1. 背景与目标

目标产品是一个运行在 macOS 上的 RDP 服务端：Windows 的 mstsc、Windows App，以及 FreeRDP、IronRDP 等客户端
能连上来看到并操作 Mac 的控制台会话。商业上先做个人免费，再做企业收费或 Pro 版本。

已经确认的事实：

- libscreenio 原型已经能在 macOS 上用 ScreenCaptureKit 枚举显示器、取帧、读光标位置与形状、按 RDP 的
  set-1 扫描码模型注入键鼠，依赖图 37 个 crate，没有 rustdesk 代码，没有 objc 0.2 / block 0.1 / CGDisplayStream。
- FreeRDP 的 macOS shadow 子系统已死：上游 2025-04 起不在 Apple 上构建，对着 macOS 26 SDK 有 6 个硬错误，
  强行编出来的二进制在监听前即 SIGTRAP。它的协议核心可用，只能作参考实现与互操作对照。
- IronRDP 服务端（`ironrdp-server` 0.13.0，2026-07-10；自 2024-11 以来 17 个版本）已具备：TLS 与 CredSSP 两种
  安全层，`CredentialValidator` 密码校验钩子，RemoteFX 与 RDP6 位图编码器，图形管线服务端可发送
  AVC420、AVC444、AVC444v2、planar、uncompressed、RemoteFX progressive、ClearCodec 帧，带帧确认与 QoE 统计，
  剪贴板、声音、显示控制、驱动器重定向、触摸、USB 通道，网络自动探测，大光标，桌面尺寸变化。
  许可 MIT 或 Apache-2.0，Devolutions 商业维护，工具链要求 Rust 1.94。

平台约束，与路线无关但决定产品边界：

- 第三方只能镜像控制台会话，不能像 Windows RDS 那样为每个用户开独立会话。
- 屏幕录制与辅助功能权限授予宿主二进制，MDM 不能直接授予屏幕录制，只能允许用户自行批准。
- 本机是 7680x4320 的 Retina 屏，CPU 编码撑不住原生分辨率的 60 fps，必须走 VideoToolbox 硬编。
- 需要屏幕录制权限的守护进程和监听端口进不了 App Store，只能 Developer ID 签名加公证分发。

## 2. 决策

**D1 协议栈用 IronRDP，不自研，不以 FreeRDP 为内核。** `ironrdp-server` 及其 crate 族提供连接序列、安全层、
编码器、通道与图形管线；我们只实现它的四个扩展点：`RdpServerDisplay`、`RdpServerInputHandler`、
`CredentialValidator`、`GfxServerFactory`，加通道工厂。FreeRDP 保留为参考实现：协商细节、编码参数、
客户端兼容性问题时对照其源码与 sample 服务器行为。

**D2 采集与注入用 libscreenio。** `screenio-core` 是唯一触碰 ScreenCaptureKit、CoreGraphics 事件、AppKit 光标的
地方，其他平台保留桩实现。它保持独立仓库与 C ABI，以便日后被别的服务端或语言使用。

**D3 编码分三步，不引入 OpenH264 或 FFmpeg。** 第一步 RemoteFX 加 RDP6 位图，全部由 IronRDP 现成编码器完成；
第二步 VideoToolbox 硬编 H.264，经图形管线以 AVC420 发送；第三步 AVC444 双流拆分与 RemoteFX progressive。
H.264 只用系统自带编码器，回避专利与 GPL 依赖。

**D4 认证与传输安全。** TLS 1.2 与 1.3 由 rustls 提供；证书首次运行自签名并存于配置目录，可替换为导入证书。
第一阶段只开 TLS 安全层，凭据来自 ClientInfo，由 `CredentialValidator` 调 OpenDirectory 或 PAM 校验本地账号，
带失败限速与锁定。NLA 等 TLS 路径稳定后加入：IronRDP 的 CredSSP 服务端做 NTLM 挑战需要预先知道凭据，因此计划两种模式都支持，
每用户的 RDP 凭据库面向独立 Mac，Kerberos 与 keytab 面向加入域的 Mac。

**D5 进程模型。** Rust 守护进程 `rdpmacd` 运行在用户登录会话中的 LaunchAgent 里，负责监听、会话、采集、编码、注入；
Swift 菜单栏 App 负责权限引导、证书与设置、状态显示、更新；两者用本机 Unix socket 通信。守护进程签名为带稳定
bundle identifier 的可执行文件，这样 TCC 授权在重新构建后仍然有效。

**D6 仓库与许可。** 产品名 rdpmac，守护进程 `rdpmacd`，仓库 `~/works/rdpmac`（Cargo workspace）。libscreenio 保持独立
仓库，Apache-2.0。rdpmac 采用与 RustDesk 相同的双许可：免费版 AGPL-3.0，Pro 版加入更多功能并以商业许可发布，
因此需要贡献者协议以保留双许可权利；Pro 功能放在闭源的独立仓库，以 crate 形式接入。libscreenio 在有远程仓库前
以路径依赖接入，之后改为固定 tag 的 git 依赖并用 `[patch]` 指向本地路径开发。不允许 rustdesk 的 AGPL 代码进入
任何一个仓库。

**D7 互操作与测试基线。** `rdp-baseline` 里已构建的 `sdl-freerdp` 和 `probe_x224.py` 作为自动化冒烟工具；
mstsc、Windows App、Microsoft Remote Desktop for Mac、FreeRDP、IronRDP client 构成手工互操作矩阵；
每个里程碑都要产出与 FreeRDP 基线同口径的数字。

## 3. 架构

### 3.1 组件

```
Windows / macOS / Linux RDP 客户端
        │  TCP 3389, TLS
        ▼
┌──────────────────────── rdpmacd（Rust，LaunchAgent）─────────────────────────┐
│ ironrdp-server：监听、X.224/MCS/GCC、TLS、能力协商、fast-path、通道、egfx │
│   ▲ DisplayUpdate            ▲ 凭据校验         │ KeyboardEvent/MouseEvent │
│   │                          │                  ▼                          │
│ rdpmac-session：帧调度、脏矩形、编码选择、光标缓存、QoE 反馈、会话状态机       │
│   ▲ Frame / CursorShape      │ rdpmac-auth     │ key_scancode / mouse_*   │
│   │                          │ OpenDirectory   ▼                          │
│ screenio-core：ScreenCaptureKit 采集 │ PAM   │ CGEvent 注入、光标、权限    │
│ rdpmac-encode：RemoteFX(IronRDP) │ VideoToolbox H.264 → AVC420/AVC444          │
└───────────────────────────────┬───────────────────────────────────────────┘
                                │ Unix socket（状态、设置、权限请求）
                    Swift 菜单栏 App（权限引导、证书、设置、更新）
```

### 3.2 数据流

- **画面。** 采集线程从 libscreenio 拉帧，带脏矩形；`rdp-session` 把脏矩形合并、按客户端能力选择路径：
  没有图形管线的客户端走 `DisplayUpdate::Bitmap`，IronRDP 内部编成 RemoteFX 或 RDP6 位图并分片；
  有图形管线且支持 H.264 的客户端走 VideoToolbox，硬编后由 `send_avc420_frame` 或 `send_avc444_frame` 发送，
  帧确认与 `should_backpressure` 决定下一帧节奏。显示器重配置时 libscreenio 返回 `Reset`，会话发
  `DisplayUpdate::Resize` 并重建采集器。
- **光标。** ScreenCaptureKit 不画光标，光标线程轮询位置与形状 id：位置变化发 `PointerPosition`，形状变化按
  尺寸发 `RGBAPointer` 或 `LargePointer`，并按 id 维护缓存索引，隐藏时发 `HidePointer`。
- **输入。** `KeyboardEvent::Pressed{code, extended}` 直通 `key_scancode(code, EXTENDED)`，`Released` 加 `RELEASE`；
  `UnicodePressed(u16)` 合并代理对后走 `key_unicode`；`Synchronize(flags)` 走新增的 `sync_locks`。
  `MouseEvent::Move` 与 `Button` 走绝对坐标，`RelMove`、`ButtonRel` 走相对坐标，滚轮值以 120 为一格直通。
  客户端断开时调用 `release_all`。
- **认证。** TLS 连接的 ClientInfo 凭据交给 `rdpmac-auth`，用 OpenDirectory 校验本地或目录账号，失败计数按来源 IP
  与用户名限速；通过后才进入会话；审计日志记录来源、账号、结果、时长。
- **控制。** 菜单栏 App 通过 Unix socket 读取守护进程状态（监听地址、当前会话、权限状态），写入设置
  （端口、绑定地址、证书、编码偏好），并在权限缺失时引导用户到系统设置。

### 3.3 运行时与线程

- `rdpmacd` 用 tokio current-thread 运行 IronRDP 事件循环，与官方示例一致；采集、编码、光标各一个专用 OS 线程，
  通过有界 channel 交给会话任务，满时丢旧帧，保证"最新帧优先"。
- libscreenio 的 `Capturer` 与 `Input` 都在各自线程持有，不跨线程共享；`Input` 只从一个注入线程调用。
- 单会话原则：同一时刻只服务一个交互客户端，其余排队或只读，由 IronRDP 的 `ConnectionPolicy` 表达。

### 3.4 接口映射

| IronRDP 扩展点 | libscreenio 或本项目实现 | 备注 |
|---|---|---|
| `RdpServerDisplay::size` | `list_displays()` 的主显示器像素尺寸 | 可选"逻辑分辨率"模式，以半分辨率服务低带宽客户端 |
| `RdpServerDisplayUpdates::next_update` | `Capturer::frame` 加脏矩形 → `BitmapUpdate{x,y,w,h,BGRA,stride}` | 无变化不产出；`Reset` → `Resize` |
| 光标更新 | `cursor_position` / `cursor_shape` → `PointerPosition`、`RGBAPointer`、`LargePointer`、`HidePointer` | 形状 id 作缓存键 |
| `RdpServerInputHandler::keyboard` | `Input::key_scancode`、`key_unicode`、`sync_locks` | 扫描码模型一致，零转换 |
| `RdpServerInputHandler::mouse` | `Input::mouse_move`、`mouse_move_rel`、`mouse_button`、`mouse_wheel` | X1/X2 已支持 |
| `CredentialValidator::validate` | `rdpmac-auth` → OpenDirectory / PAM | TLS 模式 |
| `GfxServerFactory` 与 `send_avc420_frame` 等 | `rdpmac-encode` 的 VideoToolbox 管线 | M2 起 |
| `CliprdrServerFactory` | NSPasteboard 桥 | M2 文本，M4 图片与文件 |
| `request_layout` | 显示模式切换或虚拟显示器 | M4 |
| 声音工厂 | ScreenCaptureKit 音频或 CoreAudio | M4 |

### 3.5 配置与数据

- 配置文件在 `~/Library/Application Support/rdpmac/config.toml`，证书与私钥同目录，权限 0600。
- 日志走 `tracing`，同时输出到文件与统一日志；会话审计单独文件。
- 不在磁盘保存任何客户端凭据。

## 4. 里程碑

估算按一名熟练工程师全职计，含测试与文档。

| 里程碑 | 范围 | 验收标准 | 估算 |
|---|---|---|---|
| M0 已完成 | libscreenio 原型；FreeRDP 基线；IronRDP 能力核对 | 见输入文档 | 完成 |
| M1 第一帧 | 服务端仓库骨架；IronRDP 接 libscreenio 的显示与输入；TLS 自签名；OpenDirectory 密码校验；RemoteFX；光标形状与位置；`release_all`；日志 | mstsc 与 Windows App 能连上并操作主显示器；本机 1080p 逻辑分辨率下 RemoteFX 稳定 30 fps；键盘含修饰键、鼠标含滚轮与拖拽在 Finder、终端、浏览器里正确 | 4 到 6 周 |
| M2 Retina 与体验 | libscreenio 脏矩形与 `Reset`；VideoToolbox H.264 → AVC420；帧确认背压与网络探测接入；显示器变化重建；剪贴板文本；相对鼠标 | 5K 原生分辨率 30 fps 时守护进程 CPU 低于一颗核心的 60%；局域网端到端输入延迟低于 50 ms；拔插显示器不掉线 | 3 到 4 周 |
| M3 产品外壳 | Swift 菜单栏 App；LaunchAgent 安装与卸载；权限引导；证书导入；设置界面；签名与公证；pkg 安装器；崩溃与日志收集 | 全新 Mac 上从安装到首次远程连接不需要终端；重新构建后权限不丢；公证通过 | 3 到 4 周 |
| M4 企业能力 | NLA 两种模式（凭据库、Kerberos）；AVC444 与 progressive；显示控制接口；声音；剪贴板图片与文件；MDM 托管配置；审计与会话录制接口；登录窗口会话调研 | 域账号与独立账号都能走 NLA；企业安全问卷可回答 | 8 到 12 周 |
| M5 Pro 与商业化 | 许可证与激活；更新通道；可选遥测；Pro crate 接入；多显示器实现（接口在 M1 起保留） | 免费版与 Pro 版从同一代码库构建 | 按业务排期 |

M1 到 M3 合计约 3 到 4 个月出可发布的免费版。

## 5. libscreenio 需要的改动

按里程碑归类，都是增量：

- M1：`Input::sync_locks`；Unicode 代理对；扫描码表补齐 ISO/JIS 键；用辅助功能权限实机验证修饰键、拖拽、
  滚轮方向；`Capturer` 与 `Input` 的线程约定写进文档。
- M2：透出 `SCStreamFrameInfoDirtyRects` 的脏矩形；帧时间戳；`CGDisplayRegisterReconfigurationCallback` 触发
  `Reset` 与显示器变化通知；为 VideoToolbox 提供不经 CPU 拷贝的 `CVPixelBuffer` 帧句柄，与现有 BGRA 拷贝路径并存；
  光标形状按客户端能力选择 1x 或 2x 位图。
- M3：`open_privacy_settings` 之类的权限引导辅助；cbindgen 生成头文件并冻结 1.0 的 C ABI。
- M4：显示模式切换、音频采集。多显示器采集的接口从 M1 起保留，实现归入 Pro。
- Windows 与 Linux 后端只保留接口桩，优先级最低。

## 6. IronRDP 依赖策略

- 用 crates.io 发布版，`Cargo.lock` 锁定；每月评估一次升级，0.x 的 minor 视为可能破坏 API，升级放在独立 PR。
- 需要修改时优先向上游提交，其间用 `[patch.crates-io]` 指向 fork 的固定 commit；不长期维护私有分叉。
- 工具链用 `rust-toolchain.toml` 固定，当前最低 1.94。
- 关注点：图形管线的 AVC444 拆流辅助函数是否需要自己写；progressive 编码器的完整性；多显示器布局的服务端支持（Pro）。

## 7. 测试与验收方法

- 单元：扫描码表、脏矩形合并、光标缓存、认证限速。
- 集成：`probe_x224.py` 验证协商；`sdl-freerdp` 与 `ironrdp-client` 自动连接并校验收到帧与光标；
  无权限环境下验证错误路径。
- 互操作矩阵：mstsc（Windows 10、11）、Windows App、Microsoft Remote Desktop for Mac、FreeRDP、IronRDP client，
  每个里程碑跑一遍，记录协商到的安全层与编码。
- 性能：合成画面变化的基准脚本，记录 fps、码率、守护进程 CPU、端到端输入延迟；数字与 FreeRDP 基线并排。
- 安全：TLS 配置扫描；认证暴力破解测试；依赖漏洞审计；发布前一次外部审阅。

## 8. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| IronRDP 0.x API 变化 | 升级成本 | 锁版本，按月升级，扩展点封装在 `rdpmac-session` 一层 |
| 服务端路径互操作问题（mstsc 特殊行为） | 连接失败或花屏 | 互操作矩阵每里程碑执行；FreeRDP 源码作对照；问题上游化 |
| Retina 下 RemoteFX 撑不住 | M1 体验差 | M1 提供逻辑分辨率模式；M2 硬编 |
| AVC420 文字彩边 | 观感 | M4 AVC444；文本区域走 RemoteFX 的混合帧 |
| TCC 权限流程 | 装不上、重建后失效 | 稳定签名身份；菜单栏 App 引导；文档 |
| NLA 在 macOS 上的凭据模型 | 企业接受度 | M4 前专项决策：凭据库 或 Kerberos |
| 单会话限制 | 与 Windows RDS 期待不符 | 定价与宣传明确；调研登录窗口会话 |
| 键盘布局差异（非美式布局、IME） | 输错字符 | 扫描码直通加 Unicode 回退；布局测试矩阵 |
| VideoToolbox 编码参数与延迟 | 卡顿 | 低延迟配置、实时属性、帧确认背压 |
| 许可证选择拖延 | 影响仓库拆分 | D6 里在 M1 结束前定稿 |

## 9. 备选方案

- **libscreenio + FreeRDP 混合。** 功能最全、互操作最成熟，但 C 与 FFI 边界、CMake 依赖链、200 条以上公开安全
  通告、shadow 无剪贴板、NLA 依赖 SAM、Mac 子系统已死。留作参考实现与对照，不做内核。
- **Swift 或 ObjC/C++ 从零实现。** 协议栈至少 1.5 到 2 人年才到可用，Swift 生态无 RDP 与 ASN.1 基础库。
  Swift 只做外壳，与本决策兼容。
- **自研 Rust 协议栈。** 同上的工作量问题；IronRDP 已覆盖所需能力且许可宽松。

## 10. 后果

- 好处：单一语言内核，内存安全，许可干净，能闭源，扩展点少而清晰，libscreenio 可独立演进与复用。
- 代价：受 IronRDP 路线图影响；AVC444 拆流与部分编码器细节可能要自己补；macOS 平台约束不因选型改变。
- 负责人已对第 11 节的问题给出答复，决策据此定稿。

## 11. 已决问题（2026-09-23 负责人答复）

1. 产品名 rdpmac，守护进程 `rdpmacd`，仓库 `~/works/rdpmac`。
2. 许可：与 RustDesk 相同的双许可，免费版 AGPL-3.0，Pro 版商业许可。
3. NLA：TLS 路径稳定后加入，两种模式都支持，见 D4。
4. 登录窗口会话：保留为调研项，不进入 M1 到 M3。
5. 多显示器：M1 起保留接口，实现归入 Pro（M5）。
6. Windows 与 Linux 后端：优先级最低，只保留接口不做实现。

## 12. 未来两周的具体任务

1. 建服务端仓库 `~/works/rdpmac`：workspace、`rdpmacd` 二进制、`rdpmac-session`、`rdpmac-auth`、`rdpmac-encode` 四个 crate 骨架，
   `screenio-core` 先以路径依赖接入。
2. 以 IronRDP 的 `examples/server.rs` 为模板，把显示与输入两个 trait 接到 libscreenio，先用 `with_tls`。
3. 自签名证书生成与加载；`ExactMatchCredentialValidator` 先跑通，再换 OpenDirectory 校验。
4. 用 `sdl-freerdp` 与 mstsc 各连一次，记录协商结果与 RemoteFX 在原生和半分辨率下的 fps 与 CPU。
5. libscreenio：`sync_locks`、Unicode 代理对、光标缓存 id，并给守护进程配一个带稳定 bundle identifier 的签名脚本，
   拿到屏幕录制与辅助功能权限后完成键鼠实机验证。
