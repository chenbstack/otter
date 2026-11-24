//! 资源源:gpui-kit 自带的图标 + 程序内置的系统图标(`assets/os/*.svg`,来源见该目录的 README)。
//! 内置图标用 `include_bytes!` 编进二进制,运行时不依赖外部文件。

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_kit::assets::AllAssets;

pub struct AppAssets;

const OS_ICONS: &[(&str, &[u8])] = &[
    ("os/ubuntu.svg", include_bytes!("../assets/os/ubuntu.svg")),
    ("os/debian.svg", include_bytes!("../assets/os/debian.svg")),
    ("os/centos.svg", include_bytes!("../assets/os/centos.svg")),
    ("os/rocky.svg", include_bytes!("../assets/os/rocky.svg")),
    ("os/alma.svg", include_bytes!("../assets/os/alma.svg")),
    ("os/redhat.svg", include_bytes!("../assets/os/redhat.svg")),
    ("os/fedora.svg", include_bytes!("../assets/os/fedora.svg")),
    ("os/alpine.svg", include_bytes!("../assets/os/alpine.svg")),
    ("os/arch.svg", include_bytes!("../assets/os/arch.svg")),
    ("os/openwrt.svg", include_bytes!("../assets/os/openwrt.svg")),
    ("os/freebsd.svg", include_bytes!("../assets/os/freebsd.svg")),
    ("os/linux.svg", include_bytes!("../assets/os/linux.svg")),
];

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = OS_ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        AllAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        AllAssets.list(path)
    }
}
