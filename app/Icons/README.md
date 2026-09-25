# Mac RDP 安全访问图标 · V2

本版以安全、授权、跨终端访问 Mac 桌面为中心。

## 视觉语义
- 大窗口：macOS 主机桌面，三色窗口控制点提示平台。
- 小窗口：通用 RDP 客户端，使用中性的窗口标记。
- 盾牌与闭合锁：置于 Mac 桌面内部，表达受保护的授权访问。
- 完整窗口轮廓：保持稳定、封闭的形态。蓝色与银白延续原视觉家族，薄荷绿承载安全意象。
- 菜单栏：为小尺寸单独简化为完整窗口内的闭合锁，状态附加在窗口外侧，所有外框均保持完整。

## 内容
- app/app-light.png、app/app-dark.png：1254 × 1254 PNG 视觉母稿，内置 ImageGen 生成。
- menu/：5 个 SVG 单色模板，24 × 20 画布；黑色 + 透明。
- RDPHost.xcassets/：同一套菜单栏 SVG 的 Xcode 资源目录，已标记模板渲染与保留矢量。
- preview.html：离线预览，包含浅色、深色、菜单栏及缩小效果。
- prompts.md：本版最终生成及编辑提示词。

| 素材 | 语义 | 附加形状 |
| --- | --- | --- |
| HostTemplate | 安全远程桌面品牌符号 | 无 |
| ReadyTemplate | 服务开启，等待连接 | 空心圆 |
| ActiveTemplate | 存在活动远程会话 | 实心圆 |
| PausedTemplate | 服务暂停 | 双竖线 |
| ErrorTemplate | 服务异常，需要处理 | 叹号 |

## 接入说明
菜单栏素材建议以 24 × 20 pt 为起点，在真实 macOS 菜单栏检查浅色、深色、选中和高对比度外观。为各状态提供对应的菜单文字和辅助功能标签。尺寸为本方案的设计值，并非 Apple 强制尺寸。

应用 PNG 是视觉母稿，尚未制作动态分层 .icon 或旧式 .icns。生产版本可将主桌面、客户端、盾牌、锁分别重建为矢量层，在 Icon Composer 中配置材质及外观，使用工具应用系统遮罩。预览页的 CSS 圆角仅为近似展示。

参考：
- [Apple App icons](https://developer.apple.com/design/human-interface-guidelines/app-icons)
- [Icon Composer](https://developer.apple.com/documentation/xcode/creating-your-app-icon-using-icon-composer)
- [NSImage.isTemplate](https://developer.apple.com/documentation/appkit/nsimage/istemplate)

## 已做检查
主应用图像已目视检查；PNG 尺寸、SVG/XML、资源目录 JSON、本地预览引用和压缩包完整性已检查。本次未进行 Xcode 编译、真实菜单栏或浏览器预览页截图检查。
