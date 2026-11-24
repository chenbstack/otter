//! Windows 上给所有 exe 嵌入图标(资源管理器、任务栏、开始菜单用)。其它平台什么也不做。
fn main() {
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
