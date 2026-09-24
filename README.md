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
cargo build --release
# 静态凭据，便于本机联调：
RDPMAC_LOG=info target/release/rdpmacd --listen 0.0.0.0:33389 --auth static --user test --password test
# 用本机账号登录（PAM 服务 checkpw）：
RDPMAC_LOG=info target/release/rdpmacd --listen 0.0.0.0:3389
# 不需要任何权限的合成画面，用来测客户端和编码开销：
target/release/rdpmacd --test-pattern 1920x1080 --auth static --user test --password test
# 触发屏幕录制与辅助功能的系统提示后退出：
target/release/rdpmacd --request-permissions
```

画面默认在客户端支持时走 H.264（`--codec auto`，VideoToolbox 硬件编码，经图形管线发送，不超过 4096x2304），
否则走 RemoteFX；`--codec remotefx` 固定用 RemoteFX。纯文本剪贴板默认双向共享，`--no-clipboard` 关闭。

会话分辨率默认跟随客户端（`--resolution follow-client`）：mstsc 的 `/w`、`/h`、全屏、.rdp 里的 `desktopwidth` 与
`desktopheight` 决定连接时的分辨率，启用动态分辨率时拖动窗口会实时调整。Mac 显示器尺寸不同时，画面由
ScreenCaptureKit 缩放到客户端请求的尺寸，宽高比不同时加黑边。`--resolution native` 恢复为显示器自身的像素尺寸。

首次运行会在 `~/Library/Application Support/rdpmac/` 生成自签名证书。采集需要宿主进程拥有"屏幕录制"权限，
注入需要"辅助功能"权限；没有权限时服务仍会接受连接，但客户端看不到画面。从终端启动时权限记在终端 App 上；
作为独立程序运行时记在 `rdpmacd` 的签名身份上，`scripts/sign-dev.sh` 用稳定证书签名可让授权在重新构建后保留，
`Info.plist` 已随二进制嵌入，标识符 `com.rdpmac.rdpmacd`。

## 状态

里程碑进度、实测数字和待验证项见 `docs/milestones.md`。M2 的代码部分已完成：分辨率跟随客户端、显示器替换后重新
选择、VideoToolbox H.264、按客户端积压自适应码率、纯文本剪贴板。真实屏幕、键鼠、mstsc 与跨机剪贴板还需要
权限和一台 Windows 客户机来实测。

已知与已发布的 IronRDP 0.13.0 相关的限制：光标形状超过 96 像素时不发送（大光标更新在 IronRDP 主分支上才有），
水平滚轮事件没有对应变体，鼠标按键事件不带坐标（以最近一次移动为准）。
