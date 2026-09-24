# rdpmac

运行在 macOS 上的 RDP 服务端：让 Windows 的 mstsc、Windows App 以及 FreeRDP、IronRDP 等客户端连接并操作
Mac 的控制台会话。协议栈来自 IronRDP，屏幕采集、光标与键鼠注入来自 libscreenio，设计与里程碑见
`docs/adr/0001-macos-rdp-server-on-libscreenio-and-ironrdp.md`。

```
crates/rdpmacd          守护进程：参数、TLS 证书、装配 IronRDP 服务器
crates/rdpmac-session   IronRDP 扩展点的实现：显示更新、光标、输入映射、显示器选择接口
crates/rdpmac-encode    帧到 RDP 更新的转换；VideoToolbox 路径预留在此
crates/rdpmac-auth      凭据校验：PAM（macOS 本地与目录账号）、静态凭据、失败锁定
docs/adr                架构决策记录
```

## 运行

```sh
cargo build
# 静态凭据，便于本机联调：
RDPMAC_LOG=info target/debug/rdpmacd --listen 0.0.0.0:33389 --auth static --user test --password test
# 用本机账号登录（PAM 服务 checkpw）：
RDPMAC_LOG=info target/debug/rdpmacd --listen 0.0.0.0:3389
```

首次运行会在 `~/Library/Application Support/rdpmac/` 生成自签名证书。采集需要宿主进程拥有"屏幕录制"权限，
注入需要"辅助功能"权限；没有权限时服务仍会接受连接，但客户端看不到画面。

## 状态

M1（第一帧）进行中。2026-09-24 的本机联调：`rdpmacd` 以静态凭据监听，FreeRDP 的 sdl-freerdp 完成 X.224 协商、
TLS 握手、能力交换与凭据校验，进入会话循环并干净断开；无客户端画面时守护进程 CPU 约 0.1%。因为本进程还没有
"屏幕录制"权限，帧与键鼠注入尚未实机验证。接下来：授予权限后验证画面与输入，libscreenio 补锁定键同步，
用 mstsc 与 Windows App 各连一次并记录协商结果与 RemoteFX 的帧率、CPU。

已知与已发布的 IronRDP 0.13.0 相关的限制：光标形状超过 96 像素时不发送（大光标更新在 IronRDP 主分支上才有），
水平滚轮事件没有对应变体，鼠标按键事件不带坐标（以最近一次移动为准）。
