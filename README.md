# libscreenio

屏幕采集、光标状态、键鼠注入，一个尽量小的同步 API，附带 C ABI（`libscreenio.dylib` / `.a`）。
目标是成为 RDP 服务端的采集与注入层。目前只有 macOS 有真实现，其他平台编译同一套接口并返回
`SIO_E_UNSUPPORTED`。

## 目录

```
rustdesk/                 子模块 bitworker20/rustdesk：只读的参考实现，不参与编译
crates/screenio-core/     公共 Rust API；src/macos 是 macOS 实现，src/stub.rs 是其他平台的接口桩
crates/screenio/          C ABI，include/screenio.h，examples/c 是纯 C 的调用示例
```

## macOS 实现

| 能力 | 用到的系统接口 | Rust 绑定 |
|---|---|---|
| 采集 | ScreenCaptureKit：`SCShareableContent`、`SCContentFilter`、`SCStream`，BGRA 帧 | `objc2-screen-capture-kit`、`objc2-core-media`、`objc2-core-video` |
| 显示器枚举 | CoreGraphics：`CGGetActiveDisplayList`、`CGDisplayCopyDisplayMode` | `core-graphics` |
| 光标 | `NSCursor.currentSystemCursor` 的位图与热点；`CGEventGetLocation` | `objc2-app-kit`、原生 extern |
| 键鼠注入 | `CGEventCreateKeyboardEvent` / `CGEventCreateMouseEvent` / `CGEventCreateScrollWheelEvent`，HID 层投递 | `core-graphics` |
| 权限 | `CGPreflightScreenCaptureAccess`、`AXIsProcessTrusted` | 原生 extern |

不使用已被 Apple 弃用的 CGDisplayStream，不依赖旧的 `objc` 0.2 / `block` 0.1；对象模型走 `objc2` 0.6 生态
（`objc2`、`block2`、`dispatch2` 及各框架绑定）。

采集需要"屏幕录制"权限，键鼠注入需要"辅助功能"权限。两者都授予宿主进程（终端、IDE 或最终的服务程序），
库只能查询（`sio_session_info`）和触发系统提示（`sio_session_request_permissions`）。光标位置和光标图像
不需要权限。

## 构建与运行

```sh
cargo build                                   # 得到 target/debug/libscreenio.{dylib,a}
cargo run -p screenio-core --example screenshot [--request-permissions]
sh crates/screenio/examples/c/build.sh && ./crates/screenio/examples/c/screenshot
```

## 约定

* 坐标是操作系统的虚拟桌面坐标；macOS 下是逻辑点，`sio_display_t.scale` 给出每个点对应的采集像素数。
* `Capturer::open_scaled`（C 接口 `sio_capture_open_scaled`）让 ScreenCaptureKit 在 GPU 上把画面缩放到指定尺寸，
  宽高比不同时居中加黑边。
* 帧是 BGRA、行自上而下、带 stride；`data` 指针到下一次 `sio_capture_frame` 或 `sio_capture_close` 前有效。
  `sio_capture_frame` 只在画面有变化时返回新帧，超时返回 `SIO_E_TIMEOUT`；采集流被系统停止（显示器断开等）
  返回 `SIO_E_RESET`，此时应关闭并重新打开。
* 键盘输入用 PC/AT set-1 扫描码加 E0/E1/释放标志，也就是 RDP 报文里的原样；另有 Unicode 事件。
  修饰键状态由库自己维护并附在每个事件上。
* 锁定键：`sync_locks` 按 RDP 的 TS_SYNC_EVENT 位同步；macOS 只有 Caps Lock，通过 IOKit 的 HID 系统读写状态。
* 显示器列表包含已连接但休眠的显示器，面板关掉时服务仍能寻址它。
* 虚拟显示器：`VirtualDisplay`（C 接口 `sio_virtual_display_*`）基于私有接口 CGVirtualDisplay，运行时检测是否可用，
  尺寸为 1x 像素。不接显示器的 Mac 上它替代系统的占位显示器（`placeholder` 为真的那块）成为桌面，释放后占位显示器
  以新的 id 回来。macOS 26 会把 3840x2160 定成 1920x1080，这时 `resize` 返回错误，显示器保留系统选定的尺寸。
* 光标形状带 `scale`，即位图像素与点之比，调用方按会话缩放光标时用它。
* 权限引导：`open_privacy_settings`（C 接口 `sio_open_privacy_settings`）打开"隐私与安全性"里屏幕录制或辅助功能那一页；
  `request_permissions` 负责把进程加进这两个列表。
* 光标形状按 id 缓存：`cursor_shape_id` 只读一个计数器，适合按帧轮询；id 变了再调 `cursor_shape` 取位图。
* 所有函数同步返回，`0` 成功，负数为 `SIO_E_*`。

## C ABI

`crates/screenio/include/screenio.h` 由 cbindgen 从 `crates/screenio/src/lib.rs` 生成，头文件里的注释就是那里的文档注释。
改了 C 接口后运行 `sh scripts/header.sh`，`sh scripts/header.sh --verify` 只检查头文件是否最新；需要先 `cargo install cbindgen`。

C ABI 从 1.0（`sio_version()` 返回 `0x010000`）起冻结，1.x 只做增量：

* 已有的函数、常量、类型名、参数和含义不变。
* 结构体的大小和字段布局不变：调用方按 `sizeof` 分配数组（例如给 `sio_display_list`），加字段会破坏它们。新的数据通过新函数给出。
* 新能力以新函数和新常量加入，次版本号随之增加。
* 可能出现新的负数错误码，调用方应把不认识的负数当作失败处理。

## 与 rustdesk 的关系

rustdesk 子模块用于对照：macOS 的光标读取、扫描码到 virtual keycode 的映射、DXGI / X11 / PipeWire
采集后端都可以从它那里参考或移植。macOS 侧 rustdesk 用的是 CGDisplayStream 加 enigo（objc 0.2），
本项目没有直接编译它的源码。

## 尚未做的

* 帧的 dirty rect（ScreenCaptureKit 通过 `SCStreamFrameInfoDirtyRects` 提供，尚未透出）。
* 键鼠注入的方向与修饰键行为需要在授予辅助功能权限后实机验证。
* Ctrl+Alt+Del。
* 虚拟显示器的 HiDPI 模式：显式切换模式会让系统忽略之后的设置，需要另找办法。
* Windows / Linux 后端。

## 设计决策

整个 RDP 服务端项目（rdpmac）的架构决策与里程碑计划见 `~/works/rdpmac/docs/adr/0001-macos-rdp-server-on-libscreenio-and-ironrdp.md`。
