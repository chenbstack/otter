# otter

DeskPanel 重做版:macOS 原生桌面客户端,经 SSH 管理多台服务器的终端、文件、Docker、数据库、网站,服务器上不装任何东西。
当前是外观与交互原型(GPUI Kit,纯 Rust 原生,全部 mock 数据)。

**文档**:[docs/README.md](docs/README.md)(产品定位与需求见 [docs/product/overview.md](docs/product/overview.md))

- `src/main.rs` + `src/mock.rs`:**首页外观**(三栏 + 堆栈页,全部 mock 数据)
- `src/node_form.rs`:添加 / 编辑节点弹窗
- `src/files.rs`:文件管理抽屉(服务器 / 容器,拖入上传)
- `src/bin/bench.rs`:堆栈页变高卡片列表的**滚动性能压测**

## 首页

```bash
cargo run --release                                  # 打开首页(界面程序 otter-gui;命令行程序是 otter,见 docs/modules/ctl.md)
cargo run --release -- --shot out.png --expand 3     # 离屏截图后退出(不需要屏幕录制权限)
#   可选: --node 1,2(选节点)  --plain(隐藏图表)  --page 0..6(菜单页)  --add(打开添加节点弹窗)
#   文件抽屉: --files host | --files emby/emby_av(容器)  --fcd /etc(进目录)  --fsel hosts(选中)
#   注意:锁屏时窗口收不到渲染帧,--shot 会一直卡住
```


## 运行

```bash
cargo run --release --bin bench      # 交互,默认 300 个堆栈;点卡片头展开/折叠
cargo run --release --bin bench -- 1000          # 1000 个堆栈
cargo run --release --bin bench -- 300 --bench   # 自动滚动压测 3 个场景(各 5s),打印报告后退出
DY_SCENE=2 cargo run --release --bin bench -- 300 --bench   # 只跑指定场景(0 折叠 / 1 混合 / 2 展开)
```

## 环境坑

- **必须 nightly**:`gpui-pre-macos 0.3.12` 用了 `Option::map_or_default`,stable 1.96 还没稳定。
  已用 `rust-toolchain.toml` 局部指定 nightly,不影响全局。
- **Metal 工具链**:`.cargo/config.toml` 把 `DEVELOPER_DIR` 指到完整 Xcode(CLT 里没有 metal 编译器)。
  需要先下载过 MetalToolchain 组件,见 `../DeskPanel_gpui/README.md`。

## 关键 API 事实

`v_virtual_list` 要求**提前给出每项高度**(`item_sizes`),不靠渲染后测量。
堆栈卡片高度由数据决定(折叠 / 展开 + 服务数),直接算出来即可,
不需要 gpuix 版 `VirtualStackList` 那套"测量 + 回填 + sizeKey"。
