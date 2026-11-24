//! Windows 上给所有 exe 嵌入图标(资源管理器、任务栏、开始菜单用)。其它平台什么也不做。
/// 安装程序的内嵌内容:带 `payload` 特性时把已构建好的 otter-gui / otter 压缩进 OUT_DIR(安装时再解压,
/// 安装包体积约减半);没有该特性时写两个空文件,让 `include_bytes!` 仍然能编译。
fn pack_payload() {
    use std::io::Write;
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let with = std::env::var_os("CARGO_FEATURE_PAYLOAD").is_some();
    let ext = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") { ".exe" } else { "" };
    for (src, dst) in [("otter-gui", "gui.gz"), ("otter", "cli.gz")] {
        let mut bytes = Vec::new();
        if with {
            let path = format!("target/release/{src}{ext}");
            println!("cargo:rerun-if-changed={path}");
            let data = std::fs::read(&path).unwrap_or_else(|e| panic!("payload 特性需要先构建 {path}: {e}"));
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            enc.write_all(&data).unwrap();
            bytes = enc.finish().unwrap();
        }
        std::fs::write(out.join(dst), bytes).unwrap();
    }
}

fn main() {
    pack_payload();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/logo/otter.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/logo/otter.ico");
        res.set("ProductName", "Otter");
        res.set("FileDescription", "Otter");
        if let Err(e) = res.compile() {
            println!("cargo:warning=嵌入图标失败: {e}");
        }
    }
}
