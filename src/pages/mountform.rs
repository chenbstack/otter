//! 存储页的挂载管理:「挂载」按钮、行内编辑/卸载、挂载表单。
//! 逻辑(校验、脚本)在 `crate::mounts`,这里只管界面和把表单交给后台任务。

use gpui_kit::component::input::InputState;

use super::super::*;
use super::daemonpage::{field, labeled, switch};
use super::dockerpages::DockAsk;
use super::syspages::chip;
use crate::docker::Action;
use crate::mounts::{self, MountKind, MountSpec};
use crate::sysops::{self, Job, Kind, Mount};

/// 输入框下标
const SRC: usize = 0;
const MNT: usize = 1;
const FST: usize = 2;
const OPT: usize = 3;
const USR: usize = 4;
const PWD: usize = 5;
/// 高级里的数值参数:6.. 是通用输入槽,每种类型把前几个槽对应到 `前缀=值` 形式的选项
const P0: usize = 6;
const PARAM_SLOTS: usize = 5;

/// (选项前缀, 名称, 提示, 倍数):界面填的数 × 倍数 = 写进选项的值(NFS 的 timeo 内核按十分之一秒算,界面按秒填)
fn params(kind: MountKind) -> &'static [(&'static str, &'static str, &'static str, u64)] {
    match kind {
        MountKind::Local => &[("uid=", "属主 UID", "", 1), ("gid=", "属组 GID", "", 1), ("umask=", "权限掩码", "", 1)],
        MountKind::Nfs => &[("timeo=", "超时(秒)", "", 10), ("retrans=", "重试次数", "", 1), ("rsize=", "读块大小(KB)", "", 1024), ("wsize=", "写块大小(KB)", "", 1024)],
        MountKind::Cifs => &[("uid=", "文件属主 UID", "", 1), ("gid=", "文件属组 GID", "", 1), ("file_mode=", "文件权限", "", 1), ("dir_mode=", "目录权限", "", 1), ("domain=", "域 / 工作组", "", 1)],
    }
}

#[derive(Default)]
pub(crate) struct MountUi {
    /// None 没打开;Some(None) 新增;Some(Some(挂载点)) 编辑那个挂载点
    pub(super) form: Option<Option<String>>,
    inputs: Option<Vec<Entity<InputState>>>,
    kind: MountKind,
    persist: bool,
    /// 编辑的 CIFS 挂载以前保存过凭据:用户名读不到,要重新填;密码留空沿用
    had_cred: bool,
    /// 高级区是否展开(各类型各有自己的高级项)
    pub(super) adv: bool,
}

impl Home {
    /// 打开表单:`edit` 是存储列表里的下标,None 是新增
    pub(super) fn mount_open(&mut self, edit: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.sys.mt.inputs.is_none() {
            let v = ["来源", "/mnt/data", "auto", "defaults,nofail", "用户名", "密码", "不设置", "不设置", "不设置", "不设置", "不设置"]
                .iter()
                .enumerate()
                .map(|(i, ph)| {
                    let e = cx.new(|cx| {
                        let s = InputState::new(window, cx).placeholder(*ph);
                        if i == PWD { s.masked(true) } else { s }
                    });
                    cx.observe(&e, |_, _, cx| cx.notify()).detach();
                    e
                })
                .collect();
            self.dock.sys.mt.inputs = Some(v);
        }
        let m: Option<Mount> = edit.and_then(|i| match &self.dock.sys.storage {
            Some(Ok(l)) => l.get(i).cloned(),
            _ => None,
        });
        let (kind, src, mnt, fst, opts, persist, cred) = match &m {
            Some(m) => {
                let e = m.fstab.as_ref();
                let fstype = e.map(|e| e.fstype.clone()).unwrap_or_else(|| m.fstype.clone());
                let kind = MountKind::guess(&fstype);
                let raw = e.map(|e| e.options.clone()).unwrap_or_else(|| kind.default_options().to_string());
                let cred = raw.split(',').any(|o| o.starts_with("credentials="));
                // 账号密码单独填:选项里不留 credentials / guest
                let opts: Vec<&str> = raw.split(',').filter(|o| !o.starts_with("credentials=") && *o != "guest" && !o.is_empty()).collect();
                (kind, e.map(|e| e.source.clone()).unwrap_or_else(|| m.dev.clone()), m.mount.clone(), fstype, opts.join(","), e.is_some(), cred)
            }
            None => (MountKind::Local, String::new(), String::new(), MountKind::Local.default_fstype().to_string(), MountKind::Local.default_options().to_string(), true, false),
        };
        // 数值参数从选项串里拆到各自的输入框
        let mut slots = vec![String::new(); PARAM_SLOTS];
        let mut rest: Vec<&str> = Vec::new();
        for o in opts.split(',').filter(|o| !o.is_empty()) {
            let hit = params(kind).iter().position(|(p, ..)| o.starts_with(p)).and_then(|i| {
                let (p, _, _, mult) = params(kind)[i];
                let v = &o[p.len()..];
                if mult == 1 {
                    return Some((i, v.to_string()));
                }
                // 带倍数的:除得尽才拆到输入框,否则原样留在选项里
                v.parse::<u64>().ok().filter(|n| n % mult == 0).map(|n| (i, (n / mult).to_string()))
            });
            match hit {
                Some((i, v)) => slots[i] = v,
                None => rest.push(o),
            }
        }
        let opts = rest.join(",");
        let has_param = slots.iter().any(|v| !v.is_empty());
        let mt = &mut self.dock.sys.mt;
        mt.kind = kind;
        mt.persist = persist;
        mt.had_cred = cred && kind == MountKind::Cifs;
        mt.adv = m.is_some() && (has_param || fst != kind.default_fstype() || opts != kind.default_options());
        mt.form = Some(m.as_ref().map(|m| m.mount.clone()));
        if let Some(ins) = &mt.inputs {
            for (i, v) in [(SRC, src), (MNT, mnt), (FST, fst), (OPT, opts), (USR, String::new()), (PWD, String::new())] {
                ins[i].update(cx, |s, cx| s.set_value(&v, window, cx));
            }
            for (i, v) in slots.iter().enumerate() {
                ins[P0 + i].update(cx, |s, cx| s.set_value(v, window, cx));
            }
            // 焦点放进弹窗:Esc、Enter 靠焦点在弹窗里才能收到
            let fh = ins[SRC].focus_handle(cx);
            window.focus(&fh, cx);
        }
        cx.notify();
    }

    /// 新增时换了类型:文件系统类型和选项换成该类型的默认值
    pub(super) fn mount_set_kind(&mut self, k: MountKind, window: &mut Window, cx: &mut Context<Self>) {
        let mt = &mut self.dock.sys.mt;
        if mt.kind == k {
            return;
        }
        mt.kind = k;
        mt.adv = false;
        if let Some(ins) = &mt.inputs {
            ins[FST].update(cx, |s, cx| s.set_value(k.default_fstype(), window, cx));
            ins[OPT].update(cx, |s, cx| s.set_value(k.default_options(), window, cx));
            for i in 0..PARAM_SLOTS {
                ins[P0 + i].update(cx, |s, cx| s.set_value("", window, cx));
            }
            let fh = ins[SRC].focus_handle(cx);
            window.focus(&fh, cx);
        }
        cx.notify();
    }

    /// 点了开关、选项之后把焦点放回表单里,保证 Esc / Enter 还能收到
    fn mount_refocus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ins) = &self.dock.sys.mt.inputs {
            let fh = ins[SRC].focus_handle(cx);
            window.focus(&fh, cx);
        }
    }

    /// 选项输入框里的各项(逗号分隔)
    fn mount_opts(&self, cx: &App) -> Vec<String> {
        match &self.dock.sys.mt.inputs {
            Some(ins) => ins[OPT].read(cx).value().split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
            None => vec![],
        }
    }

    fn mount_set_opts(&mut self, v: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.mount_refocus(window, cx);
        if let Some(ins) = &self.dock.sys.mt.inputs {
            ins[OPT].update(cx, |s, cx| s.set_value(&v.join(","), window, cx));
        }
        cx.notify();
    }

    /// 开关一个选项(ro 与 rw 互斥)
    fn mount_opt_toggle(&mut self, tok: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut v = self.mount_opts(cx);
        if v.iter().any(|x| x == tok) {
            v.retain(|x| x != tok);
        } else {
            if tok == "ro" {
                v.retain(|x| x != "rw");
            }
            v.push(tok.to_string());
        }
        self.mount_set_opts(v, window, cx);
    }

    /// 设置形如 `vers=3.0` 的选项;None 是去掉
    fn mount_opt_set(&mut self, prefix: &str, val: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let mut v = self.mount_opts(cx);
        v.retain(|x| !x.starts_with(prefix));
        if let Some(val) = val {
            v.push(format!("{prefix}{val}"));
        }
        self.mount_set_opts(v, window, cx);
    }

    fn mount_set_fstype(&mut self, t: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.mount_refocus(window, cx);
        if let Some(ins) = &self.dock.sys.mt.inputs {
            ins[FST].update(cx, |s, cx| s.set_value(t, window, cx));
        }
        cx.notify();
    }

    /// 表单内容 + 校验结果
    fn mount_check(&self, cx: &App) -> Result<MountSpec, String> {
        let mt = &self.dock.sys.mt;
        let ins = mt.inputs.as_ref().ok_or("表单没有准备好")?;
        let editing = matches!(mt.form, Some(Some(_)));
        let get = |i: usize| ins[i].read(cx).value().to_string();
        let kind = mt.kind;
        let mount = match &mt.form {
            Some(Some(m)) => m.clone(),
            _ => get(MNT).trim().to_string(),
        };
        let mut options = get(OPT).trim().to_string();
        for (i, (prefix, name, _, mult)) in params(kind).iter().enumerate() {
            let v = get(P0 + i).trim().to_string();
            if v.is_empty() {
                continue;
            }
            let ok = if *prefix == "domain=" { v.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_')) } else { v.chars().all(|ch| ch.is_ascii_digit()) };
            if !ok {
                return Err(format!("{name}{}", if *prefix == "domain=" { "只能填字母、数字、点、横线" } else { "只能填数字" }));
            }
            if !options.is_empty() {
                options.push(',');
            }
            let v = if *mult == 1 { v } else { v.parse::<u64>().ok().and_then(|n| n.checked_mul(*mult)).ok_or_else(|| format!("{name}数值太大"))?.to_string() };
            options.push_str(&format!("{prefix}{v}"));
        }
        let spec = MountSpec {
            kind,
            source: get(SRC).trim().to_string(),
            mount,
            fstype: if kind == MountKind::Cifs { "cifs".into() } else { get(FST).trim().to_string() },
            options,
            persist: mt.persist,
            user: if kind == MountKind::Cifs { get(USR).trim().to_string() } else { String::new() },
            password: if kind == MountKind::Cifs { get(PWD) } else { String::new() },
        };
        let existing: Vec<String> = match &self.dock.sys.storage {
            Some(Ok(l)) => l.iter().map(|m| m.mount.clone()).collect(),
            _ => Vec::new(),
        };
        mounts::validate(&spec, editing, &existing)?;
        if editing && mt.had_cred && kind == MountKind::Cifs && spec.user.is_empty() {
            return Err("这个挂载用过账号,请填写用户名(密码留空沿用以前保存的)".into());
        }
        Ok(spec)
    }

    fn mount_submit(&mut self, cx: &mut Context<Self>) {
        let spec = match self.mount_check(cx) {
            Ok(s) => s,
            Err(e) => {
                self.dock.notice = Some((e.into(), std::time::Instant::now()));
                return cx.notify();
            }
        };
        let editing = matches!(self.dock.sys.mt.form, Some(Some(_)));
        // CIFS 密码走 stdin 第一行,不进脚本、不进命令行
        let input = (spec.kind == MountKind::Cifs && !spec.user.is_empty() && !spec.password.is_empty()).then(|| format!("{}\n", spec.password));
        let job = Job {
            label: if editing { "修改挂载" } else { "挂载" }.into(),
            target: spec.mount.clone(),
            script: mounts::mount_script(&spec, editing),
            root: true,
            input,
            reload: Some((Kind::Storage, sysops::STORAGE_SCRIPT.to_string(), false)),
            guard: None,
            secs: 90,
            calm: true,
        };
        self.dock.sys.mt.form = None;
        self.sys_run(job, cx);
    }

    /// 行内「卸载」:确认后卸载并从 fstab 移除
    pub(super) fn mount_ask_remove(&mut self, m: &Mount, cx: &mut Context<Self>) {
        let job = Job {
            label: "卸载挂载".into(),
            target: m.mount.clone(),
            script: mounts::remove_script(&m.mount),
            root: true,
            input: None,
            reload: Some((Kind::Storage, sysops::STORAGE_SCRIPT.to_string(), false)),
            guard: None,
            secs: 90,
            calm: false,
        };
        let detail = if m.fstab.is_some() { "挂载点,写在 /etc/fstab 里" } else { "挂载点,临时挂载" };
        self.ask(
            DockAsk {
                title: "卸载挂载",
                tile: IconName::Unplug,
                name: m.mount.clone(),
                detail,
                body: "卸载并从 /etc/fstab 移除,不会删除里面的文件,也不会删除挂载点目录。目录正在使用时会卸载失败并提示原因,不会强制卸载",
                label: "卸载",
                action: Action::Sys(job),
                list: Vec::new(),
            },
            cx,
        );
    }

    pub(crate) fn mount_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if self.page != Page::Storage {
            return None;
        }
        let editing_mount = self.dock.sys.mt.form.clone()?;
        let ins = self.dock.sys.mt.inputs.clone()?;
        let mt = &self.dock.sys.mt;
        let editing = editing_mount.is_some();
        let kind = mt.kind;
        let check = self.mount_check(cx);
        let can = check.is_ok();
        let summary = match &check {
            Ok(s) if s.persist => format!("挂载到 {},并写入 /etc/fstab,开机自动挂载", if editing { editing_mount.clone().unwrap_or_default() } else { s.mount.clone() }),
            Ok(s) => format!("只临时挂载到 {},重启后不会自动挂载", if editing { editing_mount.clone().unwrap_or_default() } else { s.mount.clone() }),
            Err(e) => e.clone(),
        };
        // 刚打开、什么都没填时不用红字吓人
        let untouched = ins[SRC].read(cx).value().trim().is_empty() && (editing || ins[MNT].read(cx).value().trim().is_empty());
        let warn = !can && !untouched;

        let mut sections: Vec<AnyElement> = Vec::new();
        if editing {
            sections.push(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(format!("{} · 挂载点 {}", kind.label(), editing_mount.clone().unwrap_or_default()))).into_any_element());
        } else {
            let row = div().flex().flex_row().gap(px(6.0)).children([MountKind::Local, MountKind::Nfs, MountKind::Cifs].into_iter().enumerate().map(|(i, k)| {
                chip(("mt-kind", i), k.label(), kind == k).on_click(cx.listener(move |this, _, window, cx| this.mount_set_kind(k, window, cx)))
            }));
            sections.push(labeled("类型", "", row).into_any_element());
        }
        let src_label = match kind {
            MountKind::Local => "设备",
            MountKind::Nfs => "NFS 共享",
            MountKind::Cifs => "SMB 共享",
        };
        sections.push(field(src_label, kind.source_hint(), &ins[SRC]).into_any_element());
        if !editing {
            sections.push(field("挂载点", "服务器上的目录,不存在会自动创建;写绝对路径,例如 /mnt/data", &ins[MNT]).into_any_element());
        }
        if kind == MountKind::Cifs {
            sections.push(field("用户名", if mt.had_cred { "这个挂载用过账号,请重新填写用户名;留空是匿名访问" } else { "留空是匿名访问(guest)" }, &ins[USR]).into_any_element());
            sections.push(field("密码", if editing { "留空沿用以前保存的密码。密码保存在服务器 /root 下的凭据文件里,权限 600,不会出现在命令行里" } else { "保存在服务器 /root 下的凭据文件里,权限 600,不会出现在命令行里" }, &ins[PWD]).into_any_element());
        }
        let persist = mt.persist;
        sections.push(
            div()
                .id("mt-persist")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .child(switch("mt-persist-sw", persist))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child("开机自动挂载(写入 /etc/fstab)"))
                        .child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(if editing { "关掉会把 fstab 里这一行去掉,当前挂载保持,重启后不再挂" } else { "关掉只做临时挂载,重启后消失" })),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.mount_refocus(window, cx);
                    this.dock.sys.mt.persist = !this.dock.sys.mt.persist;
                    cx.notify();
                }))
                .into_any_element(),
        );

        // 高级:各类型自己的参数,默认收起
        let adv_note = match kind {
            MountKind::Local => "文件系统类型、挂载选项",
            MountKind::Nfs => "NFS 版本、挂载选项",
            MountKind::Cifs => "挂载选项",
        };
        let adv = mt.adv;
        sections.push(
            div()
                .id("mt-adv")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .text_size(px(t::TYPE_CONTROL))
                .text_color(c::secondary())
                .hover(|s| s.text_color(c::text()))
                .child(icon(if adv { IconName::ChevronDown } else { IconName::ChevronRight }, 14.0, c::ghost()))
                .child("高级")
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(adv_note))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.mount_refocus(window, cx);
                    this.dock.sys.mt.adv = !this.dock.sys.mt.adv;
                    cx.notify();
                }))
                .into_any_element(),
        );
        if adv {
            let mut a = div().flex().flex_col().gap(px(12.0)).pl(px(2.0));
            let opts = self.mount_opts(cx);
            let has = |t: &str| opts.iter().any(|x| x == t);
            let fst_now = ins[FST].read(cx).value().trim().to_string();
            // 单选一排:(显示, 值)
            let pick = |label: &'static str, hint: &'static str, items: Vec<(String, String)>, cur: String, id: &'static str, cx: &mut Context<Self>| {
                let row = div().flex().flex_row().flex_wrap().gap(px(6.0)).children(items.into_iter().enumerate().map(|(i, (l, v))| {
                    let on = v == cur;
                    chip((id, i), l, on).on_click(cx.listener(move |this, _, window, cx| {
                        if id == "mt-fst" {
                            this.mount_set_fstype(&v, window, cx);
                        } else {
                            this.mount_opt_set("vers=", if v.is_empty() { None } else { Some(v.as_str()) }, window, cx);
                        }
                    }))
                }));
                labeled(label, hint, row)
            };
            match kind {
                MountKind::Local => {
                    let mut items: Vec<(String, String)> = [("自动识别", "auto"), ("ext4", "ext4"), ("xfs", "xfs"), ("btrfs", "btrfs"), ("exFAT", "exfat"), ("NTFS", "ntfs3"), ("FAT32", "vfat")].iter().map(|(l, v)| (l.to_string(), v.to_string())).collect();
                    if !items.iter().any(|(_, v)| *v == fst_now) && !fst_now.is_empty() {
                        items.push((fst_now.clone(), fst_now.clone()));
                    }
                    a = a.child(pick("文件系统类型", "一般选「自动识别」,让系统自己判断", items, fst_now.clone(), "mt-fst", cx));
                }
                MountKind::Nfs => {
                    let items = vec![("自动协商".to_string(), "nfs".to_string()), ("NFSv4".to_string(), "nfs4".to_string())];
                    a = a.child(pick("NFS 版本", "「自动协商」由客户端和服务器商量;服务器只支持 v4 时选 NFSv4", items, fst_now.clone(), "mt-fst", cx));
                }
                MountKind::Cifs => {
                    let cur = opts.iter().find_map(|x| x.strip_prefix("vers=")).unwrap_or("").to_string();
                    let mut items: Vec<(String, String)> = [("自动", ""), ("3.1.1", "3.1.1"), ("3.0", "3.0"), ("2.1", "2.1"), ("2.0", "2.0")].iter().map(|(l, v)| (l.to_string(), v.to_string())).collect();
                    if !items.iter().any(|(_, v)| *v == cur) {
                        items.push((cur.clone(), cur.clone()));
                    }
                    a = a.child(pick("SMB 版本", "「自动」由系统协商;老设备连不上时试 2.1 或 2.0", items, cur, "mt-vers", cx));
                }
            }
            let tog = |id: &'static str, title: &'static str, sub: &'static str, tok: &'static str, on: bool, cx: &mut Context<Self>| {
                div()
                    .id(id)
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .py(px(2.0))
                    .cursor_pointer()
                    .child(switch(tok, on))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(title))
                            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(sub)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.mount_opt_toggle(tok, window, cx)))
            };
            let mut cards: Vec<Stateful<Div>> = vec![tog("mt-o-nofail", "不在时继续开机", "nofail", "nofail", has("nofail"), cx)];
            if kind != MountKind::Local {
                cards.push(tog("mt-o-net", "等网络就绪", "_netdev", "_netdev", has("_netdev"), cx));
            }
            cards.push(tog("mt-o-ro", "只读", "ro", "ro", has("ro"), cx));
            if kind != MountKind::Cifs {
                cards.push(tog("mt-o-atime", "不记访问时间", "noatime", "noatime", has("noatime"), cx));
            }
            let mut tg = div().flex().flex_col().gap(px(8.0));
            // 三个一排,不足的补空位
            let mut it = cards.into_iter().peekable();
            while it.peek().is_some() {
                let mut r = div().flex().flex_row().gap(px(10.0));
                for _ in 0..3 {
                    r = match it.next() {
                        Some(x) => r.child(x),
                        None => r.child(div().flex_1().min_w_0()),
                    };
                }
                tg = tg.child(r);
            }
            a = a.child(labeled("选项", "", tg));
            // 数值参数:每排 3 个(4 个时 2×2),说明合并成一行
            let ps = params(kind);
            let cols = if ps.len() == 4 { 2 } else { 3 };
            let mut grid = div().flex().flex_col().gap(px(8.0));
            for group in (0..ps.len()).collect::<Vec<_>>().chunks(cols) {
                let mut r = div().flex().flex_row().gap(px(10.0));
                for &i in group {
                    r = r.child(div().flex_1().min_w_0().child(field(ps[i].1, "", &ins[P0 + i])));
                }
                for _ in group.len()..cols {
                    r = r.child(div().flex_1().min_w_0());
                }
                grid = grid.child(r);
            }
            let note = match kind {
                MountKind::Local => "UID / GID / 掩码:给 exFAT、NTFS 这类没有属主的文件系统用,如 1000、022",
                MountKind::Nfs => "超时:服务器没响应多久算超时,默认 60 秒;块大小一般不用改。都不填就用系统默认",
                MountKind::Cifs => "UID / GID:文件显示为谁的,如 1000;权限如 0664、0775。都不填就用系统默认",
            };
            a = a.child(labeled("数值参数", "", grid.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(note))));
            // 其他没有对应开关的选项(例如已有挂载里的 uid=、dmask=):原样保留,只读显示
            let known = ["defaults", "nofail", "_netdev", "ro", "rw", "noatime"];
            let other: Vec<&String> = opts.iter().filter(|x| !known.contains(&x.as_str()) && !x.starts_with("vers=")).collect();
            if !other.is_empty() {
                a = a.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(format!("其他选项(原样保留):{}", other.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(",")))));
            }
            sections.push(a.into_any_element());
        }
        let title = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(14.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child(if editing { "编辑挂载" } else { "挂载存储" }))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(if editing { "先卸载再按新参数挂载;失败会自动恢复原来的挂载" } else { "用服务器自带的 mount 挂载,先试挂,成功了才改 /etc/fstab" }));
        let body = div().id("mt-body").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().gap(px(16.0)).px(px(24.0)).pb(px(20.0)).children(sections);
        let cancel = crate::node_form::ctl_btn("mt-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
            this.dock.sys.mt.form = None;
            cx.notify();
        }));
        let submit = div()
            .id("mt-ok")
            .flex()
            .flex_row()
            .items_center()
            .h(px(34.0))
            .px(px(16.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .when(can, |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
            .when(!can, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
            .child(if editing { "保存" } else { "挂载" })
            .on_click(cx.listener(|this, _, _, cx| this.mount_submit(cx)));
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .child(div().flex_1().min_w_0().text_size(px(t::TYPE_MICRO)).text_color(if warn { c::warn() } else { c::ghost() }).child(SharedString::from(if untouched { "填好来源和挂载点就可以挂载".to_string() } else { summary })))
            .child(cancel)
            .child(submit);
        Some(crate::ui::modal(
            "mount-dialog",
            560.0,
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    this.dock.sys.mt.form = None;
                    cx.notify();
                    cx.stop_propagation();
                }
                "enter" => {
                    this.mount_submit(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [title.into_any_element(), body.into_any_element(), footer.into_any_element()],
        ))
    }
}
