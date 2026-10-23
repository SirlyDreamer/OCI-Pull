# OCI Pull logo

深灰圆角矩形底色和白色 Lucide Container 图标，外侧透明。图标几何与原标题栏一致。

- `logo.svg`：可缩放矢量原稿。
- `logo.png`：512 × 512，供标题栏和预览使用。
- `logo.ico`：16、20、24、32、40、48、64、128、256 像素，供 Windows 使用。
- `logo.icns`：16–1024 像素，供 macOS 应用包使用。
- `container.svg`：Lucide Container 原始图标，来自当前 GPUI Kit 图标资源；许可证见同目录 `LICENSE-LUCIDE`。

Rust 构建会把 ICO 作为资源 ID 1 嵌入程序，GPUI Windows 后端将其用作窗口和任务栏图标。运行程序无需外部图片文件。

如需修改资源，在安装 Node.js 和 `sharp` 的开发环境运行 `node tools/generate-logo.cjs`。普通 Rust 构建使用已生成的资源，不需要 Node.js。
