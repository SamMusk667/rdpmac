# 剪贴板（M4 第 4 项）

Mac 与客户端之间共享剪贴板（MS-RDPECLIP，静态通道 CLIPRDR）。文字从 M1 起就有；图片和文件 2026-09-26 加入。
`clipboard = true | false`（`--no-clipboard`），默认打开。

## 怎么同步

每个连接一个工作线程，独占 Mac 的粘贴板：

- 每 0.5 秒看一次粘贴板的变更计数。变了，就读出文字和图片，和上次交换过的不同时，把可提供的格式告诉客户端（格式列表）；
  客户端粘贴时才来要数据，服务端这时再读粘贴板、转换、回给它。
- 客户端复制了东西，它发来格式列表，服务端马上逐个去要：先文字，再一种图片格式。通道一次只能有一个请求在等，所以按顺序要，
  全部到了再一起写进粘贴板（清空后写文字、PNG、TIFF）。等待中客户端又复制了别的，迟到的回答会被丢掉。
- 和上次交换过的相同的内容不再提供、不再写，自己写进粘贴板的也不会再提供回去，两边不会来回弹。

## 图片

- Mac → 客户端：提供 `CF_DIB` 和名为 `PNG` 的注册格式。PNG 保留透明；DIB 是 24 位、合成在白底上，因为 Windows 程序对
  32 位 DIB 第四个字节的理解不一致，有的当透明度，有的当填充。Windows 会从 `CF_DIB` 自己合成 `CF_BITMAP` 和 `CF_DIBV5`。
- 客户端 → Mac：客户端提供 `PNG` 就要 PNG（截图工具、浏览器、Office 都放），否则要 `CF_DIBV5`，再否则 `CF_DIB`。
  DIB 支持 BITMAPINFOHEADER 到 BITMAPV5HEADER，1 到 32 位，未压缩或位域，上下两种行序；32 位时第四个字节全为 0 当不透明。
  RLE 压缩和 OS/2 头不支持。
- Mac 上写 PNG 和 TIFF 两种：新程序读 PNG，老程序读 TIFF。读取时优先 PNG，否则 TIFF；颜色经 ImageIO 转到 sRGB。
- 在 Finder 里复制文件时，粘贴板上带着文件图标（TIFF），这不当图片提供。
- 图片最大约 8K×4K（3500 万像素），超过的不提供也不接收。

## 文件

客户端协商了文件传送（`CB_STREAM_FILECLIP_ENABLED`，mstsc 会）才传文件；否则在 Finder 里复制的文件照旧只传名字。
服务端声明的能力：文件传送、不带绝对路径、可锁定剪贴板、超过 4 GB 的文件。

- Mac → 客户端：Finder 里复制文件或文件夹，在资源管理器里粘贴。服务端把粘贴板上的文件 URL（Finder 给的是文件引用
  URL，先解析成路径）逐个展开：文件夹递归，文件夹本身也列出来，空文件夹也能过去；符号链接、`.DS_Store` 和 `._` 开头的
  文件跳过；Windows 不允许的字符（`<>:"/\|?*`）换成 `_`，去掉末尾的点和空格；连同相对路径超过 259 个字符的跳过；一次
  最多 1 万项。客户端按序号先问大小再按段读内容，服务端从磁盘读了回给它，同一个文件保持打开。客户端锁定剪贴板时，
  服务端记下当时的文件列表，锁定期间即使 Mac 上又复制了别的，它也照旧能读完。
- 客户端 → Mac：资源管理器里复制，在 Finder 里粘贴。客户端一复制，服务端就去要文件列表（IronRDP 解析并清理路径，
  自动锁定客户端剪贴板），接着逐个文件每次要 1 MB，写进 `~/Library/Caches/rdpmac/clipboard/<时间>/`，文件夹照原样
  建，保留修改时间；全部到齐后把顶层的文件和文件夹放上粘贴板，这时在 Finder 里粘贴。下载期间粘贴板先清空，免得
  粘贴出原来的内容。开始新的一次传送时删掉上一次的文件夹（Finder 粘贴时已经复制走了）。
- 下载中客户端又复制了别的、Mac 上又复制了别的，或者客户端 30 秒没有回应，这次传送放弃。不在目标文件夹内的路径
  （`..`、绝对路径）跳过。
- 现在是一复制就下载（"立即"）。macrdp 那样等 Finder 粘贴时才下载（"延迟"）要靠 NSFileCoordinator，以后再做；在那之前，
  在资源管理器里复制了很大的文件但不打算粘贴，也会下载一遍。

资源管理器的两个已知问题（macrdp 记录的，Windows 和 mstsc 的行为）：

- 直接复制一个文件夹，资源管理器只放 Shell IDList，`FileGroupDescriptorW` 要延迟生成，mstsc 不去要，所以什么都传不过来。
  办法：打开文件夹，Ctrl+A 全选再复制。
- 7-Zip、WinRAR 之类的压缩软件会拦截 `.zip`、`.7z`、`.rar` 等文件的复制，mstsc 收不到文件列表。办法：先改个扩展名。

IronRDP 补丁：服务端事件循环里剪贴板操作出错（比如客户端没协商文件传送时发文件列表、给已经放弃的请求回数据）原来会
断开整个会话，现在只丢掉这一条并记日志 `Dropping clipboard event`。

## 日志

- `copied on the Mac, offering it to the client chars=… picture=…`
- `copied on the client, now on the Mac chars=… picture=… written=…`
- 读不了的图片：`client clipboard picture could not be read bytes=…`
- `files copied on the Mac, offering them to the client files=… entries=…`
- `files copied on the client, fetching them entries=…`，完成时 `files copied on the client arrived entries=… bytes=… seconds=… dir=…`
  和 `files copied on the client, now on the Mac`；中途放弃 `stopped fetching files copied on the client why=…`

## 测试

- 单元测试：DIB 写出（24 位、倒序行、白底合成）与读回；V5 头带透明度按预乘读入；32 位第四字节全 0 当不透明；位域掩码在
  头后面、调色板、1 位图；截断的、RLE、OS/2 头拒绝；PNG 与 TIFF 经 ImageIO 往返。同步逻辑：Mac 图片提供 DIB 和 PNG，
  文字加图片三种格式；客户端的文字和图片按顺序要、一起写；没有 PNG 时要 DIB；拿不到图片不动粘贴板；迟到的回答丢弃。
- 真实粘贴板（用独立命名的粘贴板，不碰你的剪贴板）：文字加图片写成 PNG 和 TIFF 再读回；从 PNG 写入与原图同一身份；
  读不了的 PNG 什么都不写；复制文件时不提供图片；文件写成 URL 再读回成路径。
- 文件：一个文件夹树（含 2 MB 多、跨三段的文件，空文件夹，跳过的符号链接和 `.DS_Store`）加一个带冒号的文件，由 `Outgoing`
  回答、`Incoming` 下载，内容、结构和修改时间一致；客户端不给大小时先问大小；客户端回错误时停止；越出目标文件夹的路径
  跳过；锁定的列表在新的复制之后照旧可读，解锁后回到当前列表；同步逻辑：没协商文件传送时只传名字，协商了就提供文件并
  回答内容请求，客户端的文件先清空粘贴板、到齐后放上去且不回传，新的复制中止下载。
- 本机回环不能测：客户端和服务端共用同一个粘贴板。要用另一台机器上的 mstsc 实测。
- macOS 27 上 ImageIO 遇到不认识的数据时，`CGImageSourceCreateImageAtIndex` 直接触发陷阱退出进程，不是返回空；解码前先
  检查类型和图片数。

## 下一步

- 客户端 → Mac 的延迟下载（Finder 粘贴时才取），以及下载进度显示。
- 并发请求多段，提高高延迟链路上的速度（现在一次一段，1 MB 每个往返）。
