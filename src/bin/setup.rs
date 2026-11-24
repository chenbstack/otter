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

/// 在 Windows 上跑一段 PowerShell(不弹窗);其它平台什么也不做。
fn powershell(script: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
            .creation_flags(0x0800_0000)
            .output()
            .map_err(|e| format!("运行 PowerShell 失败:{e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
    }
    #[cfg(not(windows))]
    let _ = script;
    Ok(())
}

/// PowerShell 单引号字符串里的路径转义。
fn q(p: &Path) -> String {
    p.to_string_lossy().replace('\'', "''")
}

/// 结束正在运行的 Otter(否则文件被占用,覆盖/删除会失败)。
fn kill_running() {
    let _ = powershell("Get-Process otter-gui,otter -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep -Milliseconds 400");
}

fn set_path(dir: &Path, add: bool) -> Result<(), String> {
    let d = q(dir);
    let tail = if add { "$parts += $d; " } else { "" };
    powershell(&format!(
        "$d='{d}'; $p=[Environment]::GetEnvironmentVariable('Path','User'); $parts=@(); if($p){{$parts=@($p.Split(';') | Where-Object {{ $_ -and $_ -ne $d }})}}; {tail}[Environment]::SetEnvironmentVariable('Path',($parts -join ';'),'User')"
    ))
}

fn shortcuts(dir: &Path, desktop: bool) -> Result<(), String> {
    let d = q(dir);
    let desk = if desktop { "mk ([Environment]::GetFolderPath('Desktop'));" } else { "" };
    powershell(&format!(
        "$d='{d}'; $w=New-Object -ComObject WScript.Shell; \
         function mk($folder){{ $s=$w.CreateShortcut((Join-Path $folder 'Otter.lnk')); $s.TargetPath=(Join-Path $d 'otter-gui.exe'); $s.WorkingDirectory=$d; $s.IconLocation=(Join-Path $d 'otter.ico'); $s.Save() }}; \
         mk ([Environment]::GetFolderPath('Programs')); {desk}"
    ))
}

fn register(dir: &Path) -> Result<(), String> {
    let d = q(dir);
    powershell(&format!(
        "$d='{d}'; $k='HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Otter'; New-Item -Path $k -Force | Out-Null; \
         Set-ItemProperty $k DisplayName 'Otter'; Set-ItemProperty $k DisplayVersion '{VERSION}'; Set-ItemProperty $k Publisher 'Otter'; \
         Set-ItemProperty $k InstallLocation $d; Set-ItemProperty $k DisplayIcon (Join-Path $d 'otter.ico'); \
         Set-ItemProperty $k UninstallString ('\"' + (Join-Path $d 'uninstall.exe') + '\" --uninstall'); \
         Set-ItemProperty $k NoModify 1 -Type DWord; Set-ItemProperty $k NoRepair 1 -Type DWord"
    ))
}

#[derive(Clone)]
struct Options {
    dir: PathBuf,
    add_path: bool,
    desktop: bool,
}

const INSTALL_STEPS: &[&str] = &["关闭正在运行的 Otter", "复制程序文件", "创建快捷方式", "配置 otter 命令", "注册卸载入口"];
const UNINSTALL_STEPS: &[&str] = &["关闭正在运行的 Otter", "删除快捷方式", "移除 otter 命令", "清理卸载入口", "删除程序文件"];

fn install(o: &Options, step: &dyn Fn(usize)) -> Result<(), String> {
    step(0);
    kill_running();
    step(1);
    std::fs::create_dir_all(&o.dir).map_err(|e| format!("创建目录 {} 失败:{e}", o.dir.display()))?;
    let (gui, cli) = if cfg!(windows) { ("otter-gui.exe", "otter.exe") } else { ("otter-gui", "otter") };
    for (name, gz) in [(gui, GUI_GZ), (cli, CLI_GZ)] {
        let p = o.dir.join(name);
        let mut bytes = Vec::new();
        if !gz.is_empty() {
            std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(gz), &mut bytes).map_err(|e| format!("解压 {name} 失败:{e}"))?;
        }
        std::fs::write(&p, bytes).map_err(|e| format!("写入 {} 失败:{e}", p.display()))?;
    }
    let p = o.dir.join("otter.ico");
    std::fs::write(&p, ICON).map_err(|e| format!("写入 {} 失败:{e}", p.display()))?;
    if let Ok(me) = std::env::current_exe() {
        let _ = std::fs::copy(me, o.dir.join(if cfg!(windows) { "uninstall.exe" } else { "uninstall" }));
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

/// 卸载:最后一步删除目录。卸载程序自己就在目录里,所以交给一个延后执行的 cmd 去删。
fn uninstall(dir: &Path, step: &dyn Fn(usize)) -> Result<(), String> {
    step(0);
    kill_running();
    step(1);
    powershell("foreach($f in @([Environment]::GetFolderPath('Programs'),[Environment]::GetFolderPath('Desktop'))){ Remove-Item (Join-Path $f 'Otter.lnk') -ErrorAction SilentlyContinue }")?;
    step(2);
    set_path(dir, false)?;
    step(3);
    powershell("Remove-Item 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Otter' -Recurse -ErrorAction SilentlyContinue")?;
    step(4);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", &format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display())])
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(windows))]
    let _ = std::fs::remove_dir_all(dir);
    step(5);
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
        if self.uninstall { UNINSTALL_STEPS } else { INSTALL_STEPS }
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
                uninstall(&opts.dir, &|i| slow(i))
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
                let (title, sub) = if un { ("卸载 Otter", "将删除程序文件、快捷方式和 otter 命令".to_string()) } else { ("Otter", format!("SSH 与 Docker 管理器  ·  {VERSION}")) };
                let mut d = div().size_full().flex().flex_col().justify_between().child(hero(title, sub));
                if un {
                    d = d
                        .child(div().text_size(px(12.5)).text_color(muted()).text_center().child("你的节点和设置不会被删除,保存在应用数据目录里"))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(10.0))
                                .child(primary("go", "卸载").bg(danger()).on_click(cx.listener(|this, _, window, cx| this.start(window, cx))))
                                .child(secondary_btn("cancel", "取消").on_click(cx.listener(|_, _, _, cx| cx.quit()))),
                        );
                } else {
                    let opt = |id: &'static str, on: bool, label: &'static str, f: fn(&mut Setup)| {
                        div()
                            .id(id)
                            .h(px(40.0))
                            .px(px(12.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .cursor_pointer()
                            .child(
                                div()
                                    .size(px(18.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(5.0))
                                    .border_1()
                                    .border_color(if on { accent() } else { field_border() })
                                    .bg(if on { accent() } else { rgba(0x00000000) })
                                    .when(on, |d| d.child(Icon::new(IconName::Check).size(px(12.0)).text_color(rgb(0xFFFFFF)))),
                            )
                            .child(div().text_size(px(13.5)).text_color(secondary()).child(label))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                f(this);
                                cx.notify();
                            }))
                    };
                    let dir = self.opts.dir.display().to_string();
                    d = d
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap(px(18.0))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(8.0))
                                        .child(div().text_size(px(12.5)).font_weight(FontWeight::MEDIUM).text_color(muted()).child("安装位置"))
                                        .child(
                                            div()
                                                .w_full()
                                                .h(px(42.0))
                                                .pl(px(14.0))
                                                .pr(px(5.0))
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap(px(8.0))
                                                .rounded(px(10.0))
                                                .bg(rgba(0x00000066))
                                                .border_1()
                                                .border_color(field_border())
                                                .child(div().flex_1().min_w_0().truncate().text_size(px(13.0)).text_color(text()).child(shorten(&dir, 42)))
                                                .child(
                                                    div()
                                                        .id("pick")
                                                        .size(px(32.0))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .rounded(px(7.0))
                                                        .cursor_pointer()
                                                        .hover(|s| s.bg(rgba(0x9A94E022)))
                                                        .child(Icon::new(IconName::FolderOpen).size(px(17.0)).text_color(accent()))
                                                        .on_click(cx.listener(|this, _, window, cx| this.pick_dir(window, cx))),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .rounded(px(10.0))
                                        .border_1()
                                        .border_color(field_border())
                                        .bg(field())
                                        .py(px(4.0))
                                        .child(opt("o-path", self.opts.add_path, "把 otter 命令加入 PATH(给 agent / 终端用)", |s| s.opts.add_path = !s.opts.add_path))
                                        .child(div().h(px(1.0)).mx(px(12.0)).bg(rgba(0x9A94E016)))
                                        .child(opt("o-desk", self.opts.desktop, "创建桌面快捷方式", |s| s.opts.desktop = !s.opts.desktop)),
                                ),
                        )
                        .child(primary("go", "安装").on_click(cx.listener(|this, _, window, cx| this.start(window, cx))));
                }
                d
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
                            "Otter 已从这台电脑移除".to_string()
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

    // 应用内自动更新用:不弹界面,直接覆盖安装到 --dir(不动 PATH 和桌面快捷方式),装完启动新版
    if !uninstall && args.iter().any(|a| a == "--silent") {
        let opts = Options { dir: dir.clone(), add_path: false, desktop: false };
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
                    opts: Options { dir: dir.clone(), add_path: true, desktop: false },
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
