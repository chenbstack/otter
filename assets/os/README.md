# 系统图标

节点卡片按服务器系统显示的图标（见 `docs/modules/osinfo.md`）。

来源：[Simple Icons](https://github.com/simple-icons/simple-icons) v13.21.0 的 SVG（数据以 CC0 1.0 发布），原样复制，未修改。文件名与 `OsKind::logo()` 对应：`ubuntu` `debian` `centos` `rocky` `alma` `redhat` `fedora` `alpine` `arch` `openwrt` `freebsd` `linux`。

这些图形是各自项目的商标，这里仅用来标识服务器运行的系统。要换版本或加新系统：从同一个包取 `icons/<slug>.svg`，放进本目录，在 `src/assets.rs` 登记，在 `OsKind` 里加对应项。
