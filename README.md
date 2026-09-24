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

首次运行会在 `~/Library/Application Support/rdpmac/` 生成 TLS 自签名证书。采集需要"屏幕录制"权限，注入需要
"辅助功能"权限；没有权限时服务仍会接受连接，但客户端看不到画面、键鼠不生效，启动日志会说明 macOS 检查的是哪个进程。

## 以 LaunchAgent 运行

macOS 按"负责进程"检查这两项权限：从终端启动时是终端 App，经 SSH 启动时是 sshd，只有由 launchd 启动时才是
`rdpmacd` 自己。要用真实屏幕和键鼠，请把 `rdpmacd` 装成登录会话里的 LaunchAgent：

```sh
sh scripts/sign-dev.sh setup        # 每台 Mac 一次：创建自签名的 "rdpmac Development" 代码签名身份
cargo build --release
sh scripts/agent.sh install -- --listen 0.0.0.0:3389   # 签名、安装并启动；-- 之后的参数原样传给 rdpmacd
sh scripts/agent.sh permissions     # 让 macOS 为 rdpmacd 请求屏幕录制与辅助功能
# 在"系统设置 > 隐私与安全性"的"录屏与系统录音"和"辅助功能"里打开 rdpmacd，然后：
sh scripts/agent.sh restart
sh scripts/agent.sh status          # 运行状态、参数、签名、最近日志；logs -f 持续查看日志
```

- 二进制装在 `~/Library/Application Support/rdpmac/bin/rdpmacd`，日志写到 `~/Library/Logs/rdpmac/rdpmacd.log`。
  launchd 配置在 `~/Library/LaunchAgents/com.rdpmac.rdpmacd.plist`，进程崩溃后自动重启，登录后自动启动。
- 每次安装都用同一张证书签名，指定要求是标识符 `com.rdpmac.rdpmacd` 加证书。重新构建后再执行一次 `install`
  即可更新，不带参数时沿用上次的参数，授权保留。
- 权限记在安装路径上的 `rdpmacd` 上。以前授给 `target/release/rdpmacd` 或终端 App 的条目可以删掉。
- agent 只在用户登录到 Mac 屏幕之后运行。首次安装时 macOS 可能提示添加了后台项目，请在"系统设置 > 通用 >
  登录项与扩展"里保持允许。
- `stop` 停止到下次登录，`uninstall` 移除 agent，保留日志和 TLS 证书。
- 签名身份放在单独的钥匙串里，密码存在同目录下只有你能读的文件中，所以经 SSH 也能签名；只在签名期间把这个
  钥匙串加入搜索列表。设置 `RDPMAC_SIGN_IDENTITY` 可改用 Apple Development 等其他证书。

## 状态

里程碑进度、实测数字和待验证项见 `docs/milestones.md`。M2 的代码部分已完成：分辨率跟随客户端、显示器替换后重新
选择、VideoToolbox H.264、按客户端积压自适应码率、纯文本剪贴板。真实屏幕、键鼠、mstsc 与跨机剪贴板还需要
权限和一台 Windows 客户机来实测。

已知与已发布的 IronRDP 0.13.0 相关的限制：光标形状超过 96 像素时不发送（大光标更新在 IronRDP 主分支上才有），
水平滚轮事件没有对应变体，鼠标按键事件不带坐标（以最近一次移动为准）。
