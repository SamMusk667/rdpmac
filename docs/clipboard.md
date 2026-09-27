# 剪贴板（M4 第 4 项）

Mac 与客户端之间共享剪贴板（MS-RDPECLIP，静态通道 CLIPRDR）。文字从 M1 起就有；图片 2026-09-26 加入。文件是下一步。
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

## 日志

- `copied on the Mac, offering it to the client chars=… picture=…`
- `copied on the client, now on the Mac chars=… picture=… written=…`
- 读不了的图片：`client clipboard picture could not be read bytes=…`

## 测试

- 单元测试：DIB 写出（24 位、倒序行、白底合成）与读回；V5 头带透明度按预乘读入；32 位第四字节全 0 当不透明；位域掩码在
  头后面、调色板、1 位图；截断的、RLE、OS/2 头拒绝；PNG 与 TIFF 经 ImageIO 往返。同步逻辑：Mac 图片提供 DIB 和 PNG，
  文字加图片三种格式；客户端的文字和图片按顺序要、一起写；没有 PNG 时要 DIB；拿不到图片不动粘贴板；迟到的回答丢弃。
- 真实粘贴板（用独立命名的粘贴板，不碰你的剪贴板）：文字加图片写成 PNG 和 TIFF 再读回；从 PNG 写入与原图同一身份；
  读不了的 PNG 什么都不写；复制文件时不提供图片。
- 本机回环不能测：客户端和服务端共用同一个粘贴板。要用另一台机器上的 mstsc 实测。
- macOS 27 上 ImageIO 遇到不认识的数据时，`CGImageSourceCreateImageAtIndex` 直接触发陷阱退出进程，不是返回空；解码前先
  检查类型和图片数。

## 下一步

文件：`FileGroupDescriptorW` 文件列表加 FileContents 按需读取，两个方向。
