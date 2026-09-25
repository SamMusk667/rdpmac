# NLA（M4 第 1 项）

ADR D4 定下 NLA 支持两种模式：独立 Mac 用每用户的 RDP 凭据库，加入域的 Mac 用 Kerberos 与 keytab。凭据库模式
2026-09-24 实现并通过本机测试，2026-09-25 你用 mstsc 实测通过；Kerberos 是第二步，要等有 AD 域可测。

## 改 IronRDP 之前的验证

IronRDP 0.13 有服务端 NLA：`RdpServerBuilder::with_hybrid(acceptor, pub_key)` 让协商只接受 HYBRID 与 HYBRID_EX，
TLS 之后跑 CredSSP（sspi 的 `CredSspServer`）。rdpmacd 用静态账号预置凭据、`with_hybrid` 加
`TlsIdentityCtx::pub_key`，sdl-freerdp `/sec:nla` 回环：

| 情况 | 结果 |
|---|---|
| 正确口令 | CredSSP 完成，会话建立，H.264 26.7 fps、无丢帧 |
| 错误口令 | CredSSP 阶段拒绝，服务端 `LogonDenied: no candidate credential matched`，客户端 `ERRCONNECT_AUTHENTICATION_FAILED` |
| 只支持 TLS 的客户端 | 协商阶段拒绝，`server requires SecurityProtocol(HYBRID | HYBRID_EX)` |

缺口有三个，上游 master 至今相同：

1. **只有一个账号。** acceptor 的 `CredentialsProxyImpl` 只持有一份 `AuthIdentity`，来自 `RdpServer::set_credentials`；
   NTLM 要在客户端委派口令之前完成，服务端必须预先知道每个账号的密钥。
2. **Kerberos 传不进去。** `CredsspSequence::init` 接受 `KerberosServerConfig`，但 ironrdp-server 调
   `accept_credssp` 时写死了 `None`。
3. **委派的口令被丢弃。** CredSSP 结束时 sspi 返回客户端委派的凭据（`ServerState::Finished(identity)`），acceptor
   没有交给调用方，服务端也就无法再用 PAM 核对账号当前是否有效。

## 实现

### IronRDP 补丁

补丁在 `~/works/IronRDP` 的 `rdpmac/nla` 分支上，起点是 11a0810，也就是 crates.io 上 ironrdp 0.13 各 crate 发布时
所在的提交，源码与发布的逐字相同。rdpmac 的 `Cargo.toml` 用 `[patch.crates-io]` 把 17 个 ironrdp crate 全部指向这个
检出：只指 acceptor 和 server 的话，它们按路径依赖的兄弟 crate 会和 crates.io 上的各成一份，类型对不上。检出里的
sspi 锁到 0.21.3，与 rdpmac 和上游 master 一致。补丁只加接口，缺口 1 与 3 已补，缺口 2（Kerberos）留给第二步：

- ironrdp-acceptor：`CredsspSequence::init_with_lookup` 与 `accept_credssp_with_lookup` 接受一个 sspi
  `CredentialsProxy`，按客户端给出的用户名返回口令或 NT 哈希（`$NTLM$:` 加十六进制）；原来的单账号入口不变。CredSSP
  结束时客户端委派的凭据记入 `AcceptorResult::credentials`，形状与 ClientInfo 里的一样：UPN 整体作为用户名，
  `域\用户` 拆成用户名和域。
- ironrdp-server：`RdpServer::set_credentials_lookup`，Hybrid 连接设了查找就用它。已有的 `CredentialValidator` 现在也
  校验委派来的凭据，文档随之改写；`pub use sspi` 让调用方拿到这些类型。
- 测试：ironrdp-testsuite-extra 的端到端测试新增三个：已登记账号连上、校验器收到委派的口令；错误口令被拒；未知账号被拒。
  这三个与原有的 17 个全部通过，改动的 crate 没有新增 clippy 警告。

上游 master 的 acceptor CredSSP 代码自 11a0810 以来没有改过，这部分补丁可以原样搬过去；server.rs 变化很大，要按
master 重新整理。提交上游 PR 需要你的 GitHub 账号。

### 凭据库

- 存的是 NT 哈希：口令 UTF-16LE 编码的 MD4，16 字节，不存口令。它放在登录钥匙串的通用密码里，服务名
  `com.rdpmac.nla`，账号是 macOS 短用户名，名称显示为 "rdpmac network level authentication"。
- 钥匙串只让最后写入条目的程序免提示读取，同一用户的其他程序拿不到哈希。rdpmacd 关掉了钥匙串对话框：读不到时
  直接失败并写日志，不会在没人的 Mac 上弹窗等人。
- 签名里没有 Apple 团队 ID 时，每次构建都算另一个程序，即使用同一张自签名证书签名。2026-09-25 在登录钥匙串上实测，
  用两个构建（签名要求相同、cdhash 不同）：

  | 新构建对旧构建条目的操作 | 结果 |
  |---|---|
  | 读取 | errSecAuthFailed（-25293） |
  | 覆盖写入 | 成功；之后新构建能读，旧构建读不到 |
  | 删除 | errSecInvalidOwnerEdit（-25244），只有创建条目的那个构建能删 |

  所以升级后已登记的账号要重新登记一次，NLA 登录在此之前失败。先前说"同一证书重新签名的程序照常读取"，那是在
  `security create-keychain` 建的临时钥匙串里测的，临时钥匙串不做这项检查，结论不适用于登录钥匙串。
- rdpmacd 对此的处理：
  - 读不到时，启动日志点名这个账号，`status` 的 `nla` 报 `stale: true`、`enrolled: false`，App 的菜单和设置提示升级后
    重新登记。
  - 重新登记直接覆盖旧条目，不用先去"钥匙串访问"删除。
  - 删除登记时，条目若是更早的构建建的、删不掉，就用标记 `removed` 覆盖哈希，读出来等于没有登记；条目本身还留在
    "钥匙串访问"里，可以在那里删掉。
- 换成 Developer ID 签名后，钥匙串按团队识别程序，升级就不必重新登记。换签名身份那一次同样要重新登记。
- 登记可以在 App 设置里输入 Mac 口令完成，也可以发控制命令 `{"cmd":"nla_enroll","password":"…"}`。rdpmacd 先用配置的
  PAM 服务校验口令，空口令不会送去校验，通过后算出哈希存入钥匙串。只登记 rdpmacd 所属的用户，它服务的正是这个用户的
  控制台会话。
- `nla_remove` 删除登记。`status` 的 `nla` 字段报告用户、是否已登记、登记时间（取钥匙串条目的修改时间），以及
  `stale`：条目由更早的构建写入，本构建读不到，需要重新登记。
- 改了 Mac 口令后，旧哈希仍能通过 NTLM，但委派来的旧口令过不了 PAM，连接会被拒，这时要重新登记。

### 连接流程

1. X.224 协商只接受 HYBRID 与 HYBRID_EX，之后建立 TLS。
2. CredSSP：`NlaLookup` 先问失败锁定是否放行，再按用户名（不分大小写）从钥匙串取 NT 哈希。
3. sspi 完成 NTLMv2 验证和公钥绑定，客户端随后委派口令。
4. 校验器（失败锁定加 PAM）核对委派来的口令，通过后进入会话。

未登记的账号在第 2 步就失败，日志写明原因。

### 失败锁定

NTLM 验证失败时校验器根本不会被调用，所以 `NlaLookup` 在 NTLM 之前就向锁定模块登记一次尝试，先按失败计。校验器接受
委派来的口令后清零；拒绝时不再重复计数。锁定按去掉域、转成小写后的用户名计，换大小写拿不到更多尝试次数，TLS 模式
也随之如此。规则与 TLS 模式相同：5 分钟内失败 5 次，锁 5 分钟。

### 设置

`security = "tls" | "nla"`，可以用 `--security`、config.toml 或 App 设置来设，默认 `tls`。选了 `nla` 却没有登记任何
账号时，启动日志会警告，菜单里也会出现登记入口。`--auth static` 下 NLA 直接用静态口令的哈希，不需要登记，方便开发
联调。

## 测试（2026-09-24）

本机回环测试的服务端是 `rdpmacd --auth static --security nla --test-pattern`，数据目录和端口都单独设。客户端用
FreeRDP 3 的无界面示例客户端 `sfreerdp`，不开窗口，也不带剪贴板：

| 情况 | 结果 |
|---|---|
| 正确口令 | CredSSP 通过，校验器接受委派的口令（`Credential validation accepted`），H.264 会话持续到客户端退出 |
| 错误口令 | `LogonDenied: no candidate credential matched`，客户端 `ERRCONNECT_AUTHENTICATION_FAILED` |
| 未登记账号 | 同上，日志写明 `not enrolled` |
| 只支持 TLS 的客户端 | 协商阶段拒绝 |
| 连续 5 次失败 | 账号锁 300 秒，之后正确口令也被拒（用户名改成大写同样被拒） |

另外：

- rdpmac-auth 的钥匙串测试会写登录钥匙串，默认忽略；在图形会话里以一次性 LaunchAgent 运行时，登记、不分大小写读取、
  列出和删除都通过。经 SSH 或后台会话时登录钥匙串是锁着的，写入会失败。
- PAM 模式下的控制命令：`status` 的 `nla` 字段、`nla_remove`、空口令被拒都已验证。正确口令的登记要用你的真实口令，
  留给你在 App 里做。

### 升级后（2026-09-25）

你装上 AVC444 测试版后 mstsc 连不上（0x904，扩展 0x7）：新构建读不到 0.3.1 写入的哈希，CredSSP 失败，mstsc 随后
不带 NLA 重试也被拒。修复后的验证：

- 两个构建（签名要求相同、内容不同）在登录钥匙串的测试服务下走一遍升级：新构建先读到 `Unreadable`，哈希查找报
  "another build of rdpmacd stored the hash"；重新登记后能读，旧构建反而读不到；删除时写入标记，之后读作没有登记，
  再登记又能用；创建条目的构建最后把它删掉。
- 用新构建以独立数据目录和端口起一个服务端，只读你真实的条目：`status` 报 `enrolled: false, stale: true`，启动日志
  点名 sammusk667 需要重新登记。没有发登记或删除命令。

## 已知限制

- 自签名构建每次升级后要重新登记一次，见"凭据库"一节；App 会提示。
- HYBRID_EX 的早期认证结果在 NTLM 通过时就报成功。如果之后 PAM 拒绝，客户端收到的是 ServerDeniedConnection 断开，
  而不是"口令错误"。这只在登记后改过口令、或账号被停用时出现。
- ironrdp-server 一次只处理一个连接，钥匙串在连接的运行时线程上同步读取，耗时几毫秒。

## 下一步

1. 已完成：你在 App 里登记、打开 NLA 后用 mstsc 连接正常（2026-09-25）。改口令之后的情况还没试。
2. Kerberos：调研 macOS 绑定 AD 后机器账号密钥的来源与生成 keytab 的方式，接入 `KerberosServerConfig`，委派的凭据
   同样经 PAM 核对。需要一个 AD 域。
3. 上游 PR：按 master 整理补丁，需要你的 GitHub 账号。
4. Developer ID 签名：升级后不必重新登记，公证也需要它；需要你的 Apple 开发者账号。
