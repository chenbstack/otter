//! Otter 安装/卸载程序(Windows 为主)。
//!
//! 和应用同一套界面库,不用系统安装向导的默认样式。安装包 = 这个程序本身 + 编进去的 otter-gui.exe / otter.exe
//! (`--features payload`,CI 先构建两个二进制再构建它)。安装时把自己复制成 `uninstall.exe`,
//! 卸载入口写在「应用和功能」里,指向 `uninstall.exe --uninstall`。
//!
//! 应用内自动更新会以 `--silent --dir <安装目录>` 调用它做无界面覆盖安装。
//!
//! 调试:`otter-setup --shot out.png [--stage welcome|installing|done|error|uninstall]` 离屏截图;
//! 非 Windows 上只把文件写到临时目录,不碰系统(快捷方式、PATH、注册表只在 Windows 做)。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ::gpui::{AssetSource, Result as GpuiResult, SharedString};
use gpui_kit::assets::{AllAssets, IconName};
use gpui_kit::component::Icon;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

// ---- 安装内容 -----------------------------------------------------------------------------------
// build.rs 把 otter-gui / otter 压缩成 gzip 放进 OUT_DIR(没有 payload 特性时为空)
const GUI_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gui.gz"));
const CLI_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cli.gz"));
const ICON: &[u8] = include_bytes!("../../assets/logo/otter.ico");
const LOGO: &[u8] = include_bytes!("../../assets/logo/otter-1024.png");
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Assets;
impl AssetSource for Assets {
    fn load(&self, path: &str) -> GpuiResult<Option<Cow<'static, [u8]>>> {
        if path == "logo.png" {
            return Ok(Some(Cow::Borrowed(LOGO)));
        }
        AllAssets.load(path)
    }
    fn list(&self, path: &str) -> GpuiResult<Vec<SharedString>> {
        AllAssets.list(path)
    }
}

// ---- 色板(和应用一致)---------------------------------------------------------------------------
fn bg() -> Rgba { rgb(0x0B0A0F) }
fn text() -> Rgba { rgb(0xF5F4F7) }
fn secondary() -> Rgba { rgb(0xC9C8D0) }
fn muted() -> Rgba { rgb(0x9E9DA9) }
fn accent() -> Rgba { rgb(0x8B80FF) }
fn online() -> Rgba { rgb(0x46D9A0) }
fn danger() -> Rgba { rgb(0xFF5C6C) }
fn field() -> Rgba { rgba(0x9A94E00A) }
fn field_border() -> Rgba { rgba(0x9A94E022) }

// ---- 系统操作 ------------------------------------------------------------------------------------
fn default_dir() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"));
        base.join("Programs").join("Otter")
    } else {
        std::env::temp_dir().join("otter-setup-preview")
    }
}

/// 结束正在运行的 Otter(否则文件被占用,覆盖/删除会失败)。
fn kill_running() {
    #[cfg(windows)]
    winsys::kill_running();
}


/// 其它 Windows 系统调用(结束进程、快捷方式、提权运行)。全部直接调系统 API,不经 PowerShell:
/// 个别电脑的安全软件会拦截 PowerShell 进程(报「拒绝访问」),安装程序不依赖它。
#[cfg(windows)]
mod winsys {
    use std::path::{Path, PathBuf};
    use windows::core::{Interface, HSTRING};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, IPersistFile};
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT};

    /// 开始菜单「程序」目录或桌面目录(桌面可能被 OneDrive 重定向,所以不拼路径,问系统)。
    fn known_folder(desktop: bool) -> Option<PathBuf> {
        let id = if desktop { &FOLDERID_Desktop } else { &FOLDERID_Programs };
        // SAFETY: 返回的字符串由系统分配,读完用 CoTaskMemFree 释放
        unsafe {
            let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s.map(PathBuf::from)
        }
    }

    fn create_shortcut(lnk: &Path, dir: &Path) -> Result<(), String> {
        let err = |e: windows::core::Error| format!("创建快捷方式 {} 失败:{e}", lnk.display());
        // SAFETY: 标准的 COM 用法;安装在后台线程里跑,这个线程自己初始化 COM
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(err)?;
            link.SetPath(&HSTRING::from(dir.join("otter-gui.exe").as_path())).map_err(err)?;
            link.SetWorkingDirectory(&HSTRING::from(dir)).map_err(err)?;
            link.SetIconLocation(&HSTRING::from(dir.join("otter.ico").as_path()), 0).map_err(err)?;
            let file: IPersistFile = link.cast().map_err(err)?;
            file.Save(&HSTRING::from(lnk), true).map_err(err)?;
        }
        Ok(())
    }

    pub fn create_shortcuts(dir: &Path, desktop: bool) -> Result<(), String> {
        let start = known_folder(false).ok_or("找不到开始菜单目录")?;
        create_shortcut(&start.join("Otter.lnk"), dir)?;
        if desktop {
            let d = known_folder(true).ok_or("找不到桌面目录")?;
            create_shortcut(&d.join("Otter.lnk"), dir)?;
        }
        Ok(())
    }

    pub fn remove_shortcuts() {
        for desktop in [false, true] {
            if let Some(f) = known_folder(desktop) {
                let _ = std::fs::remove_file(f.join("Otter.lnk"));
            }
        }
    }

    /// 结束所有 otter-gui.exe / otter.exe 进程,等它们真正退出(最多 3 秒)。
    pub fn kill_running() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE};
        // SAFETY: 遍历进程快照;PROCESSENTRY32W 先清零并设置 dwSize,句柄都在用完后关闭
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]);
                if name.eq_ignore_ascii_case("otter-gui.exe") || name.eq_ignore_ascii_case("otter.exe") {
                    let h = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, e.th32ProcessID);
                    if !h.is_null() {
                        TerminateProcess(h, 1);
                        WaitForSingleObject(h, 3000);
                        CloseHandle(h);
                    }
                }
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
    }

    /// 以管理员身份运行 `exe <mode> --dir "<dir>"`(弹 UAC),等它结束,退出码非 0 或用户拒绝授权都算失败。
    pub fn run_elevated_wait(exe: &Path, mode: &str, dir: &Path) -> Result<(), String> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
        use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
        let d = dir.to_string_lossy().trim_end_matches(['\\', '/']).to_string();
        let verb = wide("runas");
        let file = wide(&exe.to_string_lossy());
        let params = wide(&format!("{mode} --dir \"{d}\""));
        // SAFETY: 结构体先清零再填必要字段,字符串缓冲区在调用期间有效;hProcess 用完关闭
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = params.as_ptr();
            info.nShow = 0; // SW_HIDE
            if ShellExecuteExW(&mut info) == 0 || info.hProcess.is_null() {
                return Err("未获得管理员授权".into());
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = 1u32;
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
            if code != 0 {
                return Err(format!("提权进程失败,退出码 {code}"));
            }
        }
        Ok(())
    }
}

/// 注册表操作直接调系统 API,不借 PowerShell:个别电脑的安全软件会拦截带「修改 PATH」命令行的 PowerShell 进程(报「拒绝访问」)。
#[cfg(windows)]
mod winreg_ops {
    use std::borrow::Cow;
    use std::path::Path;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_EXPAND_SZ};
    use winreg::{RegKey, RegValue};

    const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";

    fn to_utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().chain(Some(0)).flat_map(|u| u.to_le_bytes()).collect()
    }

    fn from_utf16(b: &[u8]) -> String {
        let v: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&v).trim_end_matches('\0').to_string()
    }

    /// 通知资源管理器等进程环境变量变了,新开的终端才读得到新 PATH。
    fn broadcast_env_change() {
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE};
        let env: Vec<u16> = "Environment".encode_utf16().chain(Some(0)).collect();
        let mut result = 0usize;
        // SAFETY: env 以 0 结尾并在调用期间有效;HWND_BROADCAST 是系统定义的广播句柄
        unsafe {
            SendMessageTimeoutW(HWND_BROADCAST as HWND, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 2000, &mut result);
        }
    }

    pub fn set_path(dir: &Path, add: bool) -> Result<(), String> {
        let err = |e: std::io::Error| format!("修改用户 PATH 失败:{e}");
        let env = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE).map_err(err)?;
        // 保留原来的值类型(多数是 REG_EXPAND_SZ,里面可能有 %USERPROFILE% 之类),没有 Path 时新建一个
        let (cur, vtype) = match env.get_raw_value("Path") {
            Ok(v) => (from_utf16(&v.bytes), v.vtype),
            Err(_) => (String::new(), REG_EXPAND_SZ),
        };
        let d = dir.to_string_lossy().to_string();
        let mut parts: Vec<&str> = cur.split(';').filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case(&d)).collect();
        if add {
            parts.push(&d);
        }
        env.set_raw_value("Path", &RegValue { bytes: Cow::Owned(to_utf16(&parts.join(";"))), vtype }).map_err(err)?;
        broadcast_env_change();
        Ok(())
    }

    pub fn register(dir: &Path, version: &str) -> Result<(), String> {
        let err = |e: std::io::Error| format!("写入卸载入口失败:{e}");
        let (k, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(format!(r"{UNINSTALL}\Otter")).map_err(err)?;
        let d = dir.to_string_lossy().to_string();
        let join = |f: &str| dir.join(f).to_string_lossy().to_string();
        k.set_value("DisplayName", &"Otter").map_err(err)?;
        k.set_value("DisplayVersion", &version).map_err(err)?;
        k.set_value("Publisher", &"Otter").map_err(err)?;
        k.set_value("InstallLocation", &d).map_err(err)?;
        k.set_value("DisplayIcon", &join("otter.ico")).map_err(err)?;
        k.set_value("UninstallString", &format!("\"{}\" --uninstall", join("uninstall.exe"))).map_err(err)?;
        k.set_value("NoModify", &1u32).map_err(err)?;
        k.set_value("NoRepair", &1u32).map_err(err)?;
        Ok(())
    }

    pub fn unregister() {
        if let Ok(k) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(UNINSTALL, KEY_READ | KEY_WRITE) {
            let _ = k.delete_subkey_all("Otter");
        }
    }
}

fn set_path(dir: &Path, add: bool) -> Result<(), String> {
    #[cfg(windows)]
    return winreg_ops::set_path(dir, add);
    #[cfg(not(windows))]
    {
        let _ = (dir, add);
        Ok(())
    }
}

fn shortcuts(dir: &Path, desktop: bool) -> Result<(), String> {
    #[cfg(windows)]
    return winsys::create_shortcuts(dir, desktop);
    #[cfg(not(windows))]
    {
        let _ = (dir, desktop);
        Ok(())
    }
}

fn register(dir: &Path) -> Result<(), String> {
    #[cfg(windows)]
    return winreg_ops::register(dir, VERSION);
    #[cfg(not(windows))]
    {
        let _ = dir;
        Ok(())
    }
}

#[derive(Clone)]
struct Options {
    dir: PathBuf,
    add_path: bool,
    desktop: bool,
    /// 卸载时同时删除应用数据(节点、设置、日志和凭据库里的主密钥);默认不删
    purge: bool,
}

const INSTALL_STEPS: &[&str] = &["关闭正在运行的 Otter", "复制程序文件", "创建快捷方式", "配置 otter 命令", "注册卸载入口"];
const UNINSTALL_STEPS: &[&str] = &["关闭正在运行的 Otter", "删除快捷方式", "移除 otter 命令", "清理卸载入口", "删除程序文件"];
const UNINSTALL_PURGE_STEPS: &[&str] = &["关闭正在运行的 Otter", "删除快捷方式", "移除 otter 命令", "清理卸载入口", "删除应用数据", "删除程序文件"];

/// 把程序文件写进安装目录(解压内置的两个程序、图标、卸载程序)。
fn write_files(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录 {} 失败:{e}", dir.display()))?;
    let (gui, cli) = if cfg!(windows) { ("otter-gui.exe", "otter.exe") } else { ("otter-gui", "otter") };
    for (name, gz) in [(gui, GUI_GZ), (cli, CLI_GZ)] {
        let p = dir.join(name);
        let mut bytes = Vec::new();
        if !gz.is_empty() {
            std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(gz), &mut bytes).map_err(|e| format!("解压 {name} 失败:{e}"))?;
        }
        std::fs::write(&p, bytes).map_err(|e| format!("写入 {} 失败:{e}", p.display()))?;
    }
    let p = dir.join("otter.ico");
    std::fs::write(&p, ICON).map_err(|e| format!("写入 {} 失败:{e}", p.display()))?;
    if let Ok(me) = std::env::current_exe() {
        let _ = std::fs::copy(me, dir.join(if cfg!(windows) { "uninstall.exe" } else { "uninstall" }));
    }
    Ok(())
}

/// 当前用户能不能写这个目录(会顺手创建它);只在「没有权限」时返回 false,其它错误留给后面写文件时报。
fn can_write(dir: &Path) -> bool {
    match std::fs::create_dir_all(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return false,
        Err(_) => return true,
        Ok(()) => {}
    }
    let probe = dir.join(".otter-write-test");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(e) => e.kind() != std::io::ErrorKind::PermissionDenied,
    }
}

/// 以管理员身份重新运行自己(弹系统的 UAC 授权窗口),做完 `mode` 指定的事,等它结束。用户点了「否」或写入失败都返回错误。
fn run_elevated(mode: &str, dir: &Path) -> Result<(), String> {
    let fail = || format!("没有拿到管理员权限,或写入 {} 失败。请在授权窗口里点「是」,或换一个不需要管理员权限的安装位置", dir.display());
    #[cfg(windows)]
    {
        let me = std::env::current_exe().map_err(|e| format!("找不到安装程序自身:{e}"))?;
        winsys::run_elevated_wait(&me, mode, dir).map_err(|_| fail())
    }
    #[cfg(not(windows))]
    {
        let _ = (mode, fail);
        Err(format!("没有权限写入 {}", dir.display()))
    }
}

/// 提权后的自己干的活(没有界面,退出码 0 成功):`--elevated-copy` 写程序文件,`--elevated-remove` 删安装目录。
fn elevated_main(mode: &str, dir: &Path) -> Result<(), String> {
    kill_running();
    if mode == "--elevated-copy" {
        write_files(dir)
    } else {
        // 只删确实是 Otter 安装目录的地方,不让这个提权入口被用来删任意目录
        let is_otter = dir.join("otter-gui.exe").exists() && dir.join("uninstall.exe").exists();
        if !is_otter {
            return Err(format!("{} 不是 Otter 的安装目录", dir.display()));
        }
        std::fs::remove_dir_all(dir).map_err(|e| format!("删除 {} 失败:{e}", dir.display()))
    }
}

fn install(o: &Options, step: &dyn Fn(usize)) -> Result<(), String> {
    step(0);
    kill_running();
    step(1);
    // 目标目录当前用户写不进去(比如 C:\\Program Files):只把写文件这一步交给管理员权限的自己做,
    // 快捷方式、PATH、卸载入口属于当前用户,仍在这里写(提权的可能是另一个账号,HKCU 不一样)
    if cfg!(windows) && !can_write(&o.dir) {
        run_elevated("--elevated-copy", &o.dir)?;
    } else {
        write_files(&o.dir)?;
    }
    step(2);
    shortcuts(&o.dir, o.desktop)?;
    step(3);
    if o.add_path {
        set_path(&o.dir, true)?;
    }
    step(4);
    register(&o.dir)?;
    step(5);
    Ok(())
}

/// 删除应用数据:数据 / 配置 / 缓存目录,以及系统凭据库里的主密钥。删掉后已保存的节点、密码、私钥都无法找回。
fn purge_data() -> Result<(), String> {
    if let Some(d) = directories::ProjectDirs::from("", "", "otter") {
        for p in [d.data_dir(), d.config_dir(), d.cache_dir()] {
            match std::fs::remove_dir_all(p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("删除 {} 失败:{e}", p.display())),
            }
            // 上一层(Windows 上是 %APPDATA%\otter)空了就一起删,非空时忽略
            if let Some(parent) = p.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
    // 主密钥不存在算成功;凭据库不可用时不阻断卸载
    if let Ok(entry) = keyring::Entry::new("otter", "master-key") {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(format!("删除系统凭据库里的主密钥失败:{e}")),
        }
    }
    Ok(())
}

/// 卸载:最后一步删除目录。卸载程序自己就在目录里,所以交给一个延后执行的 cmd 去删。
fn uninstall(dir: &Path, purge: bool, step: &dyn Fn(usize)) -> Result<(), String> {
    step(0);
    kill_running();
    step(1);
    #[cfg(windows)]
    winsys::remove_shortcuts();
    step(2);
    set_path(dir, false)?;
    step(3);
    #[cfg(windows)]
    winreg_ops::unregister();
    step(4);
    if purge {
        purge_data()?;
        step(5);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if can_write(dir) {
            let _ = std::process::Command::new("cmd")
                .args(["/C", &format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display())])
                .creation_flags(0x0800_0000)
                .spawn();
        } else {
            // 装在需要管理员权限的目录(如 Program Files):请求授权后由提权的自己删除
            run_elevated("--elevated-remove", dir)?;
        }
    }
    #[cfg(not(windows))]
    let _ = std::fs::remove_dir_all(dir);
    step(if purge { 6 } else { 5 });
    Ok(())
}

// ---- 界面 ---------------------------------------------------------------------------------------
#[derive(Clone, PartialEq)]
enum Stage {
    Welcome,
    Working,
    Done,
    Failed(String),
}

struct Progress {
    step: usize,
    result: Option<Result<(), String>>,
}

struct Setup {
    uninstall: bool,
    opts: Options,
    stage: Stage,
    progress: Arc<Mutex<Progress>>,
    shot: Option<(String, u32)>,
}

impl Setup {
    fn steps(&self) -> &'static [&'static str] {
        match (self.uninstall, self.opts.purge) {
            (true, true) => UNINSTALL_PURGE_STEPS,
            (true, false) => UNINSTALL_STEPS,
            _ => INSTALL_STEPS,
        }
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stage = Stage::Working;
        let prog = Arc::new(Mutex::new(Progress { step: 0, result: None }));
        self.progress = prog.clone();
        let opts = self.opts.clone();
        let un = self.uninstall;
        let p2 = prog.clone();
        std::thread::spawn(move || {
            let step = |i: usize| p2.lock().unwrap().step = i;
            // 给每一步留一点可见的时间,否则瞬间完成,进度条一闪而过
            let slow = |i: usize| {
                step(i);
                std::thread::sleep(Duration::from_millis(220));
            };
            let r = if un {
                uninstall(&opts.dir, opts.purge, &|i| slow(i))
            } else {
                install(&opts, &|i| slow(i))
            };
            p2.lock().unwrap().result = Some(r);
        });
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(60)).await;
                let finished = this
                    .update(cx, |this, cx| {
                        let done = this.progress.lock().unwrap().result.clone();
                        if let Some(r) = done {
                            this.stage = match r {
                                Ok(()) => Stage::Done,
                                Err(e) => Stage::Failed(e),
                            };
                        }
                        cx.notify();
                        this.stage != Stage::Working
                    })
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// 选一个父目录,程序装到它下面的 Otter 文件夹(选的本来就叫 Otter 就直接用)。
    fn pick_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("选择安装位置".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(picked) = paths.into_iter().next() else { return };
            let dir = if picked.file_name().is_some_and(|n| n.eq_ignore_ascii_case("otter")) { picked } else { picked.join("Otter") };
            let _ = this.update(cx, |this, cx| {
                this.opts.dir = dir;
                cx.notify();
            });
        })
        .detach();
    }

    fn launch(&self, cx: &mut Context<Self>) {
        let exe = self.opts.dir.join(if cfg!(windows) { "otter-gui.exe" } else { "otter-gui" });
        let _ = std::process::Command::new(exe).spawn();
        cx.quit();
    }
}

/// 路径太长时中间省略,保留头尾(盘符和最后的目录名最有用)。
fn shorten(p: &str, max: usize) -> String {
    let chars: Vec<char> = p.chars().collect();
    if chars.len() <= max {
        return p.to_string();
    }
    let head = max / 3;
    let tail = max - head - 1;
    format!("{}…{}", chars[..head].iter().collect::<String>(), chars[chars.len() - tail..].iter().collect::<String>())
}

/// 带说明的复选框卡片:左边方框,右边标题 + 说明(说明里的 `\n` 换行)。`warn` 时勾选后整张卡片变红(卸载删数据用)。
fn check_row(id: &'static str, on: bool, warn: bool, title: &'static str, desc: &'static str) -> Stateful<Div> {
    let tone = if warn { danger() } else { accent() };
    div()
        .id(id)
        .w_full()
        .px(px(14.0))
        .py(px(13.0))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(12.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(if on { tone.opacity(0.6) } else { field_border() })
        .bg(if on && warn { tone.opacity(0.08) } else { field() })
        .cursor_pointer()
        .hover(|s| s.border_color(tone.opacity(0.6)))
        .child(
            div()
                .mt(px(1.0))
                .size(px(18.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.0))
                .border_1()
                .border_color(if on { tone } else { field_border() })
                .bg(if on { tone } else { rgba(0x00000000) })
                .when(on, |d| d.child(Icon::new(IconName::Check).size(px(12.0)).text_color(rgb(0xFFFFFF)))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(div().text_size(px(14.0)).font_weight(FontWeight::MEDIUM).text_color(text()).child(title))
                .child(div().text_size(px(12.5)).line_height(px(18.0)).text_color(if on && warn { danger() } else { muted() }).child(desc)),
        )
}

fn primary(id: &'static str, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(40.0))
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(10.0))
        .bg(accent())
        .text_color(rgb(0xFFFFFF))
        .text_size(px(14.0))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|s| s.opacity(0.88))
        .child(label.into())
}

fn secondary_btn(id: &'static str, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(40.0))
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(10.0))
        .bg(field())
        .border_1()
        .border_color(field_border())
        .text_color(secondary())
        .text_size(px(14.0))
        .cursor_pointer()
        .hover(|s| s.bg(rgba(0x9A94E014)))
        .child(label.into())
}

fn hero(title: impl Into<SharedString>, sub: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(6.0))
        .child(img("logo.png").size(px(76.0)).rounded(px(18.0)))
        .child(div().mt(px(10.0)).text_size(px(22.0)).font_weight(FontWeight::BOLD).text_color(text()).child(title.into()))
        .child(div().text_size(px(13.0)).text_color(muted()).child(sub.into()))
}

impl Render for Setup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((path, frames)) = &mut self.shot {
            *frames += 1;
            if *frames == 40 {
                match window.render_to_image() {
                    Ok(img) => {
                        let _ = img.save(&*path);
                        println!("截图已保存: {path}");
                    }
                    Err(e) => println!("离屏渲染失败: {e}"),
                }
                cx.quit();
            } else {
                window.request_animation_frame();
            }
        }
        let un = self.uninstall;
        let body: Div = match self.stage.clone() {
            Stage::Welcome => {
                let btn = |id: &'static str, label: &'static str| primary(id, label);
                if un {
                    let on = self.opts.purge;
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .justify_between()
                        .child(hero("卸载 Otter", "将删除程序文件、快捷方式和 otter 命令"))
                        .child(
                            check_row(
                                "o-purge",
                                on,
                                true,
                                "同时删除我的数据",
                                if on {
                                    "节点、设置、日志，以及已保存的服务器密码和私钥都会被删除，无法恢复"
                                } else {
                                    "节点、设置、日志，以及已保存的服务器密码和私钥\n不勾选则全部保留，重新安装后可以继续使用"
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.opts.purge = !this.opts.purge;
                                cx.notify();
                            })),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(10.0))
                                .child(btn("go", "卸载").bg(danger()).on_click(cx.listener(|this, _, window, cx| this.start(window, cx))))
                                .child(secondary_btn("cancel", "取消").on_click(cx.listener(|_, _, _, cx| cx.quit()))),
                        )
                } else {
                    let dir = self.opts.dir.display().to_string();
                    // 安装位置:一行路径 + 「更改」按钮,下面一行小字说明权限
                    let location = div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(div().px(px(2.0)).text_size(px(12.5)).font_weight(FontWeight::MEDIUM).text_color(muted()).child("安装位置"))
                        .child(
                            div()
                                .id("pick")
                                .w_full()
                                .h(px(46.0))
                                .pl(px(14.0))
                                .pr(px(6.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(10.0))
                                .rounded(px(10.0))
                                .bg(field())
                                .border_1()
                                .border_color(field_border())
                                .cursor_pointer()
                                .hover(|s| s.border_color(accent().opacity(0.6)))
                                .child(Icon::new(IconName::Folder).size(px(16.0)).text_color(muted()))
                                .child(div().flex_1().min_w_0().truncate().text_size(px(13.0)).text_color(text()).child(shorten(&dir, 40)))
                                .child(
                                    div()
                                        .h(px(32.0))
                                        .px(px(12.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(7.0))
                                        .bg(accent().opacity(0.14))
                                        .text_size(px(12.5))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(accent())
                                        .child("更改"),
                                )
                                .on_click(cx.listener(|this, _, window, cx| this.pick_dir(window, cx))),
                        )
                        .child(div().px(px(2.0)).text_size(px(12.0)).text_color(muted()).child("默认装到当前用户目录；选择需要权限的位置时会请求管理员授权"));
                    let options = div()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(
                            check_row("o-path", self.opts.add_path, false, "把 otter 命令加入 PATH", "供终端和 AI agent 直接调用，新开的终端里生效")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.opts.add_path = !this.opts.add_path;
                                    cx.notify();
                                })),
                        )
                        .child(
                            check_row("o-desk", self.opts.desktop, false, "创建桌面快捷方式", "开始菜单里的快捷方式始终会创建")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.opts.desktop = !this.opts.desktop;
                                    cx.notify();
                                })),
                        );
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .justify_between()
                        .child(hero("Otter", format!("SSH 与 Docker 管理器  ·  {VERSION}")))
                        .child(div().flex().flex_col().gap(px(20.0)).child(location).child(options))
                        .child(btn("go", "安装").on_click(cx.listener(|this, _, window, cx| this.start(window, cx))))
                }
            }
            Stage::Working => {
                let steps = self.steps();
                let i = self.progress.lock().unwrap().step.min(steps.len());
                let frac = i as f32 / steps.len() as f32;
                let label = steps.get(i).copied().unwrap_or("即将完成");
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .child(hero(if un { "正在卸载…" } else { "正在安装…" }, label))
                    .child(
                        div()
                            .w_full()
                            .h(px(6.0))
                            .rounded_full()
                            .bg(field_border())
                            .child(div().h_full().w(relative(frac.max(0.04))).rounded_full().bg(accent())),
                    )
            }
            Stage::Done => {
                let mut d = div().size_full().flex().flex_col().justify_between();
                d = d.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .size(px(76.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(online().opacity(0.14))
                                .border_1()
                                .border_color(online().opacity(0.4))
                                .child(Icon::new(IconName::Check).size(px(34.0)).text_color(online())),
                        )
                        .child(div().mt(px(10.0)).text_size(px(22.0)).font_weight(FontWeight::BOLD).text_color(text()).child(if un { "已卸载" } else { "安装完成" }))
                        .child(div().text_size(px(13.0)).text_color(muted()).child(if un {
                            if self.opts.purge { "Otter 及其数据已从这台电脑移除" } else { "Otter 已从这台电脑移除,数据已保留" }.to_string()
                        } else if self.opts.add_path {
                            "新开的终端里可以直接用 otter 命令".to_string()
                        } else {
                            "可以从开始菜单打开 Otter".to_string()
                        })),
                );
                if un {
                    d.child(primary("close", "完成").on_click(cx.listener(|_, _, _, cx| cx.quit())))
                } else {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .child(primary("launch", "启动 Otter").on_click(cx.listener(|this, _, _, cx| this.launch(cx))))
                            .child(secondary_btn("close", "关闭").on_click(cx.listener(|_, _, _, cx| cx.quit()))),
                    )
                }
            }
            Stage::Failed(msg) => div()
                .size_full()
                .flex()
                .flex_col()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .size(px(76.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(danger().opacity(0.14))
                                .border_1()
                                .border_color(danger().opacity(0.4))
                                .child(Icon::new(IconName::X).size(px(34.0)).text_color(danger())),
                        )
                        .child(div().mt(px(10.0)).text_size(px(22.0)).font_weight(FontWeight::BOLD).text_color(text()).child(if un { "卸载失败" } else { "安装失败" }))
                        .child(div().max_w(px(380.0)).max_h(px(80.0)).overflow_hidden().text_size(px(12.5)).text_color(muted()).text_center().child(msg)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(primary("retry", "重试").on_click(cx.listener(|this, _, window, cx| this.start(window, cx))))
                        .child(secondary_btn("close", "关闭").on_click(cx.listener(|_, _, _, cx| cx.quit()))),
                ),
        };
        let close = div()
            .id("x")
            .absolute()
            .top_0()
            .right_0()
            .w(px(46.0))
            .h(px(36.0))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted())
            .hover(|s| s.bg(danger()).text_color(rgb(0xFFFFFF)))
            .window_control_area(WindowControlArea::Close)
            .on_click(cx.listener(|_, _, _, cx| cx.quit()))
            .child(Icon::new(IconName::X).size(px(14.0)));
        div()
            .size_full()
            .relative()
            .bg(bg())
            .child(div().absolute().top_0().left_0().right_0().h(px(36.0)).window_control_area(WindowControlArea::Drag))
            .when(cfg!(not(target_os = "macos")), |d| d.child(close))
            .child(div().size_full().px(px(28.0)).pt(px(40.0)).pb(px(28.0)).child(body))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let uninstall = args.iter().any(|a| a == "--uninstall");
    let shot = arg("--shot");
    let stage = arg("--stage");
    // 卸载时装到哪就是自己所在的目录
    let dir = if uninstall {
        std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(default_dir)
    } else {
        default_dir()
    };
    // 卸载程序运行时自己在目录里,先复制到临时目录再跑,这样才删得掉
    #[cfg(windows)]
    if uninstall && !args.iter().any(|a| a == "--from-temp") {
        use std::os::windows::process::CommandExt;
        if let Ok(me) = std::env::current_exe() {
            let tmp = std::env::temp_dir().join("otter-uninstall.exe");
            if std::fs::copy(&me, &tmp).is_ok() {
                let _ = std::process::Command::new(&tmp).args(["--uninstall", "--from-temp", "--dir"]).arg(me.parent().unwrap()).creation_flags(0x0800_0000).spawn();
                return;
            }
        }
    }
    let dir = arg("--dir").map(PathBuf::from).unwrap_or(dir);

    // 提权后的自己:不弹界面,只做一件事,用退出码告诉上面的进程成败
    if let Some(mode) = ["--elevated-copy", "--elevated-remove"].into_iter().find(|m| args.iter().any(|a| a == m)) {
        std::process::exit(if elevated_main(mode, &dir).is_ok() { 0 } else { 1 });
    }

    // 应用内自动更新用:不弹界面,直接覆盖安装到 --dir(不动 PATH 和桌面快捷方式),装完启动新版
    if !uninstall && args.iter().any(|a| a == "--silent") {
        let opts = Options { dir: dir.clone(), add_path: false, desktop: false, purge: false };
        if install(&opts, &|_| {}).is_ok() {
            let _ = std::process::Command::new(dir.join(if cfg!(windows) { "otter-gui.exe" } else { "otter-gui" })).spawn();
        }
        return;
    }

    gpui_kit::application().with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(480.0), px(580.0)), cx))),
            // 不用系统标题栏:顶部自绘一条拖动区 + 关闭按钮(macOS 上保留红绿灯)
            titlebar: Some(TitlebarOptions {
                title: Some(if uninstall { "卸载 Otter" } else { "安装 Otter" }.into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.0), px(16.0))),
            }),
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, move |_, cx| {
            cx.new(|_| {
                let mut s = Setup {
                    uninstall,
                    opts: Options { dir: dir.clone(), add_path: true, desktop: false, purge: false },
                    stage: Stage::Welcome,
                    progress: Arc::new(Mutex::new(Progress { step: 0, result: None })),
                    shot: shot.clone().map(|p| (p, 0)),
                };
                match stage.as_deref() {
                    Some("installing") => {
                        s.progress.lock().unwrap().step = 2;
                        s.stage = Stage::Working;
                    }
                    Some("done") => s.stage = Stage::Done,
                    Some("error") => s.stage = Stage::Failed("写入 C:\\Users\\me\\AppData\\Local\\Programs\\Otter\\otter-gui.exe 失败:拒绝访问".into()),
                    _ => {}
                }
                s
            })
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
