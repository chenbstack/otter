# Otter logo

- `otter.svg`:源文件(1024×1024,圆角方块)。背景沿用程序窗口底 `ui::backdrop`:墨黑底 + 左上玫红、右上紫、下方蓝三团光晕,外加一圈淡描边和顶边亮线;图形是叠层卡片,材质用程序自己的磨砂玻璃(`card_shell`):靛紫底、淡描边、顶边亮线。后面两层是节点卡片(图标块 + 一条线),前面是终端卡:红黄绿三点(`danger`/`warn`/`online`)、粗的渐变提示符 `>` 和三个点,下面托一团强调色光晕。为了小尺寸(Dock、32 像素)看得清,只用粗形状,不画细线和小字。后面两层是节点卡片(图标块 + 名称/地址两行),前面是终端卡:红黄绿三点(`danger`/`warn`/`online`)、渐变提示符 `>` 和渐隐的三个点,下面托一团强调色光晕。
- `otter-1024.png`、`otter.icns`:由 `otter.svg` 导出(`qlmanage -t -s 1024` 渲染,再按 SVG 的圆角 rx=230 抠掉四角成透明,`sips` 缩放,`iconutil` 打包;`otter.ico` 由 PNG 生成)。改了 SVG 要重新导出。
- 开发期用 `scripts/dev-app.sh` 把 release 二进制打成 `target/Otter.app`(带这个图标)并签名启动。正式打包同理:`otter.icns` 放进 `Contents/Resources`,`Info.plist` 里设 `CFBundleIconFile`。
