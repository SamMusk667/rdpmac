# rdpmac

运行在 macOS 上的 RDP 服务端：让 Windows 的 mstsc、Windows App 以及 FreeRDP、IronRDP 等客户端连接并操作
Mac 的控制台会话。协议栈来自 IronRDP，屏幕采集、光标、键鼠注入与虚拟显示器来自 libscreenio，设计与里程碑见
`docs/adr/0001-macos-rdp-server-on-libscreenio-and-ironrdp.md`。

```
crates/rdpmacd          守护进程：参数与设置文件、TLS 证书、控制套接字、装配 IronRDP 服务器
crates/rdpmac-session   IronRDP 扩展点的实现：显示更新、虚拟显示器、光标、输入映射、剪贴板、H.264 管线
crates/rdpmac-encode    帧到 RDP 更新的转换、VideoToolbox H.264、码率控制
crates/rdpmac-auth      凭据校验：PAM（macOS 本地与目录账号）、静态凭据、失败锁定、NLA 凭据库（钥匙串里的 NT 哈希）
app/                    Swift 菜单栏 App，打包后内含 rdpmacd；app/Icons 是图标设计稿
scripts/                签名、安装为 LaunchAgent、打包 App 与安装器、公证
docs/                   架构决策记录与里程碑
```

## 构建前提

NLA 需要给 IronRDP 打补丁（见 `docs/nla.md`）。补丁在与本仓库并列的 IronRDP 检出里，`Cargo.toml` 的
`[patch.crates-io]` 按路径引用它，所以构建前要先有这个检出：

```sh
git clone https://github.com/Devolutions/IronRDP ../IronRDP   # 然后切到 rdpmac/nla 分支
```

libscreenio 同样以路径依赖放在 `../libscreenio`。

## 安装 App

```sh
sh scripts/sign-dev.sh setup        # 每台 Mac 一次：创建自签名的 "rdpmac Development" 代码签名身份
sh scripts/build-app.sh --pkg       # 得到 build/rdpmac.app 与 build/rdpmac-VERSION.pkg
```

安装器把 rdpmac.app 装进"应用程序"并打开它。欢迎窗口带着走完三步：打开服务、允许"录屏与系统录音"、允许
"辅助功能"，然后显示客户端要连接的地址和证书指纹。菜单栏图标里有状态、当前连接、设置、证书导入、重启服务、
日志和诊断包。图标右下角的标记表示状态：空心圆是在等连接，实心圆是有客户端连着，双竖线是服务关着，叹号是需要处理
（缺权限、等登录项批准、开了 NLA 却没登记），没有标记是服务正在启动。

图标来自 `app/Icons`（设计稿 V2，说明见其中的 README）。打包时 `scripts/icons.swift` 从 `app/app-light.png` 按 macOS
图标的圆角方形和投影生成 AppIcon.icns，把 `menu/*.svg` 转成矢量 PDF 作菜单栏模板图；换设计稿只需替换这个目录。
深色母稿 `app-dark.png` 暂未使用，等做 Icon Composer 的 .icon 时用。

- 服务由 launchd 在登录会话里运行，崩溃后自动重启。用 Developer ID 签名、带 Team ID 的 App 通过 SMAppService
  注册服务；没有 Team ID 的构建（开发构建、自行编译的免费版）改用 `~/Library/LaunchAgents` 里的经典 LaunchAgent，
  因为 macOS 不会启动这类 App 通过 SMAppService 注册的辅助程序。
- 两项权限记在 App 里的 `rdpmacd` 上。换了安装位置就要重新授权。
- `scripts/agent.sh` 装的开发用 agent 与 App 的服务同名、同端口，不能同时存在：先 `sh scripts/agent.sh uninstall`。
- `rdpmac.app/Contents/MacOS/rdpmac --enable-server | --disable-server | --server-status | --collect-diagnostics`
  可以不开菜单完成同样的操作，便于脚本和支持。

发行版：设置 `RDPMAC_SIGN_IDENTITY`（Developer ID Application）和 `RDPMAC_INSTALLER_IDENTITY`（Developer ID Installer）
后运行 `build-app.sh --pkg`，签名会带上 hardened runtime 与安全时间戳；再用 `RDPMAC_NOTARY_PROFILE` 指向
`xcrun notarytool store-credentials` 保存的凭据运行 `sh scripts/notarize.sh`，完成公证与装订。

## 直接运行守护进程

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
否则走 RemoteFX。客户端支持时 H.264 用 AVC444，带完整色度，彩色文字没有色边；宽度不是 16 的倍数的尺寸用 AVC420。
`--codec avc420` 只用 AVC420，编码开销约减半；`--codec remotefx` 固定用 RemoteFX（见 `docs/avc444.md`）。
AVC444 的颜色转换默认分到几个核上做，`--parallel-conversion false` 只用一个核。在 App 的设置里改 AVC444、AVC420 与
颜色转换，不重启服务端，从下一次连接起生效。
H.264 按规范用全范围 BT.709；画面停下约 0.2 秒后，服务端
用更细的量化参数把当前画面再编一次，文字接近无损，变化中的画面则按码率取量化参数。剪贴板的文字、图片和文件默认双向共享（见 `docs/clipboard.md`），
`--no-clipboard` 关闭。Mac 正在播放的声音默认传到客户端（16 位立体声 PCM，44.1 kHz），`--no-audio` 关闭；客户端播放期间
Mac 本机静音，`--mute-mac false` 关闭；发送的声音不超过实时，客户端最小化时不发，免得它越放越晚（见 `docs/audio.md`）。
采集按 `--fps`（默认 30）限速，画面变得更快时中间的帧被跳过，最后一帧不丢。
mstsc 最小化时画面暂停，不采集也不发送；恢复或客户端要求重画时发完整画面。`--h264-dump` 把发出的 H.264 码流原样录下，
用来排查客户端花屏（见 `docs/refresh.md`）。

会话分辨率默认跟随客户端（`--resolution follow-client`）：mstsc 的 `/w`、`/h`、全屏、.rdp 里的 `desktopwidth` 与
`desktopheight` 决定连接时的分辨率，启用动态分辨率时拖动窗口会实时调整。

- 没接显示器的 Mac 上，会话得到一块自己的虚拟显示器，尺寸就是客户端请求的像素尺寸，替代系统的 1920x1080
  占位显示器成为桌面，原生分辨率、不缩放。最后一个会话结束 30 秒后虚拟显示器移除，占位显示器回来。
  `--virtual-display off` 关闭这一行为。
- 3840x2160 第一次会被 macOS 定成 1920x1080。rdpmacd 这时以辅助进程把显示器切到 4K，效果与在系统设置里手动切换
  相同；macOS 按显示器身份记住后，以后的 4K 会话直接就是 4K。
- 接了显示器，或 macOS 不接受请求的尺寸，画面由 ScreenCaptureKit 缩放到请求的尺寸，宽高比不同时加黑边。
- 锁屏时也能连上并解锁：会话开始时 rdpmacd 向系统声明用户活动（效果与 `caffeinate -u` 相同），屏幕点亮、锁屏界面
  出来，在 RDP 里输入口令即可解锁。收到远程键鼠时也会声明（最多每 2 秒一次），因为 macOS 不把注入的事件当作能
  启动解锁流程的用户活动：不声明的话，锁屏照样显示口令框，却对任何口令都不校验就判错。
- `--resolution native` 恢复为显示器自身的像素尺寸。

首次运行会在 `~/Library/Application Support/rdpmac/` 生成 TLS 自签名证书。采集需要"屏幕录制"权限，注入需要
"辅助功能"权限；没有权限时服务仍会接受连接，但客户端看不到画面、键鼠不生效，启动日志会说明 macOS 检查的是哪个进程，
每个连接接入时也会写明缺辅助功能权限。服务运行中才授权的，要重启服务才生效：菜单栏 App 这时会提示并给出重启按钮。

### 设置文件与控制套接字

`~/Library/Application Support/rdpmac/config.toml` 保存设置，键名与命令行参数相同（`listen`、`auth`、`security`、`pam-service`、
`codec`、`parallel-conversion`、`clipboard`、`audio`、`mute-mac`、`audio-rate`、`resolution`、`virtual-display`、`fps`、`cursor-hz`、`cert`、`key` 与 `h264-dump`），命令行给出的值优先，
`--config` 可以换文件。未知的键和越界的值会让启动失败并报出原因。

同目录下的 `control.sock` 是给菜单栏 App 用的控制套接字，只允许同一用户连接，每行一个 JSON 请求：`status`、
`request_permissions`、`get_config`、`set_config`、`import_certificate`、`nla_enroll`、`nla_remove`、`restart`。例如：

```sh
printf '%s\n' '{"cmd":"status"}' | nc -U ~/Library/Application\ Support/rdpmac/control.sock
```

导入证书要求 X.509 v3 证书和对应的私钥（PEM），旧的一对保留为 `*.previous.pem`，重启服务后生效。状态里的
SHA-1 与 SHA-256 指纹就是 mstsc 询问是否信任时显示的指纹。

### 网络级身份验证（NLA）

`--security nla`（或设置里的 `security = "nla"`，App 设置中的"Require Network Level Authentication"）让客户端在
会话建立之前先证明知道口令，服务端也要证明知道这个账号，客户端才交出口令。这样假冒的服务端拿不到口令。只接受
支持 NLA 的客户端，mstsc 和 Windows App 默认都支持。默认仍是 `tls`。

NTLM 要求服务端事先持有账号的 NT 哈希，所以用 NLA 之前要先登记：在 App 设置里输入 Mac 口令，rdpmacd 用 PAM 校验后，
把哈希存进登录钥匙串。钥匙串只让 rdpmacd 读取这个条目。改了 Mac 口令后要重新登记。认证通过后，客户端交来的口令
仍会经 PAM 核对；失败锁定对 NTLM 阶段的失败同样计数。`--auth static` 下 NLA 用静态口令，不需要登记。细节见
`docs/nla.md`。

### 日志

以 launchd 服务运行时，`RDPMAC_LOG_DIR` 指定日志目录（`~/` 开头表示主目录），守护进程每天写一个
`rdpmacd.YYYY-MM-DD.log`，保留 14 天，崩溃信息也写进去；`launchd.log` 只记录日志初始化之前的输出。在终端里直接
运行时日志打到终端。

## 开发用 LaunchAgent

macOS 按"负责进程"检查这两项权限：从终端启动时是终端 App，经 SSH 启动时是 sshd，只有由 launchd 启动时才是
`rdpmacd` 自己。开发时可以不打包 App，直接把 `target/release/rdpmacd` 装成登录会话里的 LaunchAgent：

```sh
sh scripts/sign-dev.sh setup        # 每台 Mac 一次
cargo build --release
sh scripts/agent.sh install -- --listen 0.0.0.0:3389   # 签名、安装并启动；-- 之后的参数原样传给 rdpmacd
sh scripts/agent.sh permissions     # 让 macOS 为 rdpmacd 请求屏幕录制与辅助功能
# 在"系统设置 > 隐私与安全性"的"录屏与系统录音"和"辅助功能"里打开 rdpmacd，然后：
sh scripts/agent.sh restart
sh scripts/agent.sh status          # 运行状态、参数、签名、最近日志；logs -f 持续查看日志
```

- 二进制装在 `~/Library/Application Support/rdpmac/bin/rdpmacd`，日志在 `~/Library/Logs/rdpmac/`。
  launchd 配置在 `~/Library/LaunchAgents/com.rdpmac.rdpmacd.plist`，进程崩溃后自动重启，登录后自动启动。
- 每次安装都用同一张证书签名，指定要求是标识符 `com.rdpmac.rdpmacd` 加证书。重新构建后再执行一次 `install`
  即可更新，不带参数时沿用上次的参数，授权保留。
- agent 只在用户登录到 Mac 屏幕之后运行。首次安装时 macOS 可能提示添加了后台项目，请在"系统设置 > 通用 >
  登录项与扩展"里保持允许。
- `stop` 停止到下次登录，`uninstall` 移除 agent，保留日志和 TLS 证书。
- 签名身份放在单独的钥匙串里，密码存在同目录下只有你能读的文件中，所以经 SSH 也能签名；只在签名期间把这个
  钥匙串加入搜索列表。设置 `RDPMAC_SIGN_IDENTITY` 可改用 Apple Development 等其他证书。

## 状态

里程碑进度、实测数字和待验证项见 `docs/milestones.md`。M3 的开发工作已完成：自建虚拟显示器、菜单栏 App、服务安装与卸载、
权限引导、证书导入、设置、日志与诊断包、签名与安装器；公证脚本就绪，需要 Apple 开发者账号才能跑通。M4 进行中：
NLA 的凭据库模式已完成，Kerberos 模式等有 AD 域再做。

已知与已发布的 IronRDP 0.13.0 相关的限制：光标形状超过 96 像素时不发送（大光标更新在 IronRDP 主分支上才有），
水平滚轮事件没有对应变体，鼠标按键事件不带坐标（以最近一次移动为准）。
