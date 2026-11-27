//! 计划任务的新增/编辑窗口:先选任务类型(备份数据库 / 自定义命令),再用「每天 03:00」「每隔 30 分钟」这样的控件选运行时间,
//! 不直接写 cron 表达式(自定义命令留一个「高级」入口)。
//!
//! 备份数据库:选实例、选库、选保留份数;脚本生成在服务器上(`dbinst::sched_script`),cron 行用行尾注释记录选择,见 `docs/modules/pages.md`。
//! 备份任务一律以 root 运行:脚本放 `/opt/otter/databases/bin/`,计划写 `/etc/cron.d/otter-db-backup-<容器>-<编号>`;需要登录用户能 sudo(免密或节点里保存了密码)。

use std::time::Instant;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::super::*;
use super::daemonpage::{field, labeled};
use super::syspages::chip;
use crate::crontasks::{self, PruneOpts};
use crate::dbinst::{self, DbInstance};
use crate::sysops::{self, Freq, FreqKind, Job, Kind};

#[derive(Clone, Copy, PartialEq, Default)]
pub(crate) enum TaskType {
    #[default]
    Backup,
    /// Docker 清理
    Prune,
    /// 网站模块装的证书续期(只能改时间,不能新建)
    Cert,
    Custom,
}

/// 表单状态(输入框之外的部分)
#[derive(Default)]
pub(crate) struct CronUi {
    ty: TaskType,
    freq: Freq,
    /// 直接写 cron 表达式
    advanced: bool,
    /// 备份的数据库容器
    inst: Option<String>,
    /// 选中的库
    dbs: Vec<String>,
    /// 实例里读到的库(只含能定时备份的):Ok(库名) / Err(原因)
    all: Option<Result<Vec<String>, String>>,
    /// 库名不合规(含 `-` 之类)而没列出的个数
    skipped: usize,
    /// 正在读库列表的请求标记
    loading: Option<String>,
    keep: u32,
    /// 任务编号:新建时生成,编辑沿用;旧版本建的任务没有编号,保存时补一个
    id: String,
    /// 编辑的是 `/etc/cron.d` 里的任务(root 装的)
    file: Option<String>,
    /// Docker 清理的选项
    prune: PruneOpts,
}

const PRUNE_DAYS: [u32; 5] = [0, 1, 3, 7, 30];
const KEEPS: [u32; 4] = [3, 7, 14, 30];
const MIN_EVERY: [u32; 5] = [1, 5, 10, 15, 30];
const HOUR_EVERY: [u32; 7] = [1, 2, 3, 4, 6, 8, 12];
/// 周几的显示顺序(周一在前)和 cron 里的编号
const WEEK: [(&str, u32); 7] = [("周一", 1), ("周二", 2), ("周三", 3), ("周四", 4), ("周五", 5), ("周六", 6), ("周日", 0)];
const FREQS: [(&str, FreqKind); 6] = [("每隔几分钟", FreqKind::Minutes), ("每隔几小时", FreqKind::Hours), ("每天", FreqKind::Daily), ("每周", FreqKind::Weekly), ("每月", FreqKind::Monthly), ("开机时", FreqKind::Reboot)];

/// 新任务编号:取当前时间的低位,6 位字母数字
fn new_task_id() -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64;
    format!("{:06x}", (n ^ (n >> 24)) & 0xff_ffff)
}

fn hhmm(h: u32, m: u32) -> String {
    format!("{h:02}:{m:02}")
}

impl Home {
    /// 能定时备份的数据库实例(不含 Redis)
    fn cron_insts(&self) -> Vec<DbInstance> {
        dbinst::detect(&self.dock.snap.containers).into_iter().filter(|d| d.kind.backup_ext().is_some()).collect()
    }

    fn cron_inst(&self) -> Option<DbInstance> {
        let name = self.dock.sys.cf.inst.as_deref()?;
        self.cron_insts().into_iter().find(|d| d.container.name == name)
    }

    /// 打开新增/编辑表单(`line` 是要改的那一行;`sched`、`cmd` 是它现有的内容)
    pub(super) fn cron_open(&mut self, line: Option<usize>, sched: &str, cmd: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.sys.cron_inputs.is_none() {
            let v = ["*/5 * * * *", "/usr/local/bin/backup.sh", "03:00", "1"]
                .iter()
                .map(|ph| {
                    let e = cx.new(|cx| InputState::new(window, cx).placeholder(*ph));
                    cx.observe(&e, |_, _, cx| cx.notify()).detach();
                    e
                })
                .collect();
            self.dock.sys.cron_inputs = Some(v);
        }
        let insts = self.cron_insts();
        let mut cf = CronUi { keep: 7, id: new_task_id(), ..Default::default() };
        let task = dbinst::parse_sched(cmd).filter(|t| insts.iter().any(|d| d.container.name == t.ctr));
        let ptask = crontasks::parse_prune(cmd);
        let other = ptask.is_some();
        if let Some(t) = &ptask {
            cf.ty = TaskType::Prune;
            cf.prune = t.opts;
            cf.id = t.id.clone();
        }
        match &task {
            _ if other => {}
            Some(t) => {
                cf.ty = TaskType::Backup;
                cf.inst = Some(t.ctr.clone());
                cf.dbs = t.dbs.clone();
                cf.keep = t.keep;
                if !t.id.is_empty() {
                    cf.id = t.id.clone();
                }
            }
            None if line.is_some() || !cmd.is_empty() => cf.ty = TaskType::Custom,
            None if insts.is_empty() => cf.ty = TaskType::Prune,
            None => {
                cf.ty = TaskType::Backup;
                cf.inst = insts.first().map(|d| d.container.name.clone());
            }
        }
        match Freq::from_cron(sched) {
            Some(f) => cf.freq = f,
            None if !sched.is_empty() => cf.advanced = true,
            None => {}
        }
        let freq = cf.freq;
        let (adv, cmd_text) = (if cf.advanced { sched.to_string() } else { String::new() }, if task.is_some() || other { String::new() } else { cmd.to_string() });
        self.dock.sys.cf = cf;
        if let Some(ins) = &self.dock.sys.cron_inputs {
            ins[0].update(cx, |s, cx| s.set_value(&adv, window, cx));
            ins[1].update(cx, |s, cx| s.set_value(&cmd_text, window, cx));
            ins[2].update(cx, |s, cx| s.set_value(&hhmm(freq.hour, freq.minute), window, cx));
            ins[3].update(cx, |s, cx| s.set_value(&freq.day.to_string(), window, cx));
        }
        self.dock.sys.cron_form = Some(line);
        cx.notify();
    }

    /// 编辑 `/etc/cron.d` 里 root 装的备份任务
    pub(super) fn cron_open_file(&mut self, file: &str, sched: &str, cmd: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.cron_open(None, sched, cmd, window, cx);
        let cf = &mut self.dock.sys.cf;
        cf.file = Some(file.to_string());
        if crontasks::is_cert_renew(file, cmd) {
            cf.ty = TaskType::Cert;
            cf.advanced = false;
            cf.freq.kind = FreqKind::Daily;
            let ins = self.dock.sys.cron_inputs.clone();
            if let (Some((h, m)), Some(ins)) = (sched.split_whitespace().collect::<Vec<_>>().get(..2).and_then(|v| Some((v[1].parse::<u32>().ok()?, v[0].parse::<u32>().ok()?))), ins) {
                self.dock.sys.cf.freq.hour = h;
                self.dock.sys.cf.freq.minute = m;
                ins[2].update(cx, |s, cx| s.set_value(&hhmm(h, m), window, cx));
            }
        }
    }

    fn cron_notice(&mut self, msg: &'static str, cx: &mut Context<Self>) {
        self.dock.notice = Some((msg.into(), Instant::now()));
        cx.notify();
    }

    /// 备份类型:选好实例后读库列表(每次渲染前调用,收结果、发请求)
    pub(super) fn cron_pump(&mut self) {
        if self.dock.sys.cron_form.is_none() || self.dock.sys.cf.ty != TaskType::Backup {
            return;
        }
        let Some(inst) = self.cron_inst() else { return };
        let tag = format!("cron:dbs:{}", inst.container.name);
        if self.dock.sys.cf.loading.as_deref() == Some(&tag) {
            if let Some(r) = self.sys_docker().and_then(|d| d.db_take(&tag)) {
                let cf = &mut self.dock.sys.cf;
                cf.loading = None;
                cf.all = Some(r.map(|o| {
                    let rows = dbinst::parse_dbs(inst.kind, &o);
                    let usable: Vec<String> = rows.iter().filter(|r| !r.system && dbinst::valid_ident(&r.name)).map(|r| r.name.clone()).collect();
                    cf.skipped = rows.iter().filter(|r| !r.system).count() - usable.len();
                    usable
                }));
            }
            return;
        }
        if self.dock.sys.cf.all.is_none() && self.dock.sys.cf.loading.is_none() {
            if !inst.container.running() {
                self.dock.sys.cf.all = Some(Err("这个数据库容器没有在运行,读不了库列表".into()));
                return;
            }
            if let Some(d) = self.sys_docker() {
                d.db_run(&tag, inst.kind.exec_args(&inst.container.name), Some(inst.kind.list_dbs_sql().to_string()));
                self.dock.sys.cf.loading = Some(tag);
            }
        }
    }

    /// 表单里选的运行时间:cron 表达式
    fn cron_sched(&self, cx: &App) -> Result<String, String> {
        let ins = self.dock.sys.cron_inputs.as_ref().ok_or("")?;
        let cf = &self.dock.sys.cf;
        if cf.advanced {
            let s = ins[0].read(cx).value().split_whitespace().collect::<Vec<_>>().join(" ");
            return if s.is_empty() { Err("还没有填计划".into()) } else if sysops::valid_schedule(&s) { Ok(s) } else { Err("还不是合法的计划:要 5 段(分 时 日 月 周),或 @daily 这样的简写".into()) };
        }
        let mut f = cf.freq;
        if matches!(f.kind, FreqKind::Daily | FreqKind::Weekly | FreqKind::Monthly) {
            let (h, m) = sysops::parse_hhmm(&ins[2].read(cx).value()).ok_or("时间要写成 03:00 这样(24 小时制)")?;
            (f.hour, f.minute) = (h, m);
        }
        if f.kind == FreqKind::Monthly {
            f.day = ins[3].read(cx).value().trim().parse().map_err(|_| "每月几号要填数字".to_string())?;
        }
        if f.kind == FreqKind::Weekly && f.weekdays & 0x7f == 0 {
            return Err("至少选一个星期几".into());
        }
        f.to_cron().ok_or_else(|| "运行时间不合法".to_string())
    }

    fn cron_submit(&mut self, cx: &mut Context<Self>) {
        let (Some(line), Some(ins)) = (self.dock.sys.cron_form, self.dock.sys.cron_inputs.clone()) else { return };
        let sched = match self.cron_sched(cx) {
            Ok(s) => s,
            Err(e) => {
                self.dock.notice = Some((e.into(), Instant::now()));
                return cx.notify();
            }
        };
        let Some(Ok(info)) = &self.dock.sys.cron else { return };
        if self.dock.sys.cf.ty == TaskType::Custom {
            let cmd = ins[1].read(cx).value().trim().to_string();
            if cmd.is_empty() || cmd.contains(['\n', '\r']) {
                return self.cron_notice("命令不能为空,也不能换行", cx);
            }
            let text = sysops::crontab_replace(&info.user_text, line, Some(&format!("{sched} {cmd}")));
            self.dock.sys.cron_form = None;
            return self.cron_write(if line.is_some() { "修改任务" } else { "添加任务" }, text, cx);
        }
        let reload = Some((Kind::Cron, sysops::CRON_SCRIPT.to_string(), false));
        match self.dock.sys.cf.ty {
            TaskType::Cert => {
                let Some((h, m)) = sched.split_whitespace().collect::<Vec<_>>().get(..2).and_then(|v| Some((v[1].parse::<u32>().ok()?, v[0].parse::<u32>().ok()?))) else {
                    return self.cron_notice("续期只能设成每天固定时间", cx);
                };
                let job = Job { label: "修改证书续期时间".into(), target: "crontab".into(), script: crate::web::cron_script(m, h), root: true, input: None, reload, guard: None, secs: 0, calm: false };
                self.dock.sys.cron_form = None;
                return self.sys_run(job, cx);
            }
            TaskType::Prune => {
                let cf = &self.dock.sys.cf;
                let id = cf.id.clone();
                let script = crontasks::prune_script(&cf.prune).and_then(|body| crontasks::install(&format!("prune-{id}.sh"), &body, &crontasks::prune_crond(&id), &sched, &crontasks::prune_cmd("@@BIN@@", &id, &cf.prune)));
                match script {
                    Ok(script) => {
                        let job = Job { label: "保存计划任务".into(), target: "crontab".into(), script, root: true, input: None, reload, guard: None, secs: 0, calm: false };
                        self.dock.sys.cron_form = None;
                        self.dock.sys.cron_row = None;
                        self.sys_run(job, cx);
                    }
                    Err(e) => {
                        self.dock.notice = Some((e.into(), Instant::now()));
                        cx.notify();
                    }
                }
                return;
            }
            _ => {}
        }
        // 备份数据库
        let Some(inst) = self.cron_inst() else { return self.cron_notice("先选一个数据库实例", cx) };
        let cf = &self.dock.sys.cf;
        let order = cf.all.as_ref().and_then(|r| r.as_ref().ok());
        let mut dbs: Vec<String> = match order {
            Some(all) => all.iter().filter(|d| cf.dbs.contains(d)).cloned().collect(),
            None => cf.dbs.clone(),
        };
        // 已选但这次没读到的(库被删了)不留
        dbs.dedup();
        if dbs.is_empty() {
            return self.cron_notice("至少选一个要备份的库", cx);
        }
        let (ctr, keep, id) = (inst.container.name.clone(), cf.keep, cf.id.clone());
        let body = match inst.kind.sched_script(&ctr, &id, &dbs, keep) {
            Ok(b) => b,
            Err(e) => {
                self.dock.notice = Some((e.into(), Instant::now()));
                return cx.notify();
            }
        };
        // 备份任务一律以 root 运行、装在 /opt/otter 和 /etc/cron.d:不受登录用户的 docker 权限影响,脚本和备份都在 /opt/otter 下
        let cmd = dbinst::sched_cmd("@@BIN@@", &ctr, &id, &dbs, keep);
        let mut script = match dbinst::sched_install_root(&ctr, &id, &body, &sched, &cmd) {
            Ok(s) => s,
            Err(e) => {
                self.dock.notice = Some((e.into(), Instant::now()));
                return cx.notify();
            }
        };
        // 旧版本(没有编号)的 cron.d 任务改了之后换了文件名:旧文件删掉,免得重复执行
        if let Some(old) = cf.file.as_ref().filter(|f| **f != dbinst::sched_crond_file(&ctr, &id) && !f.contains(['\'', '\n'])) {
            script.push_str(&format!("rm -f '{old}'\n"));
        }
        let job = Job { label: "保存备份任务".into(), target: "crontab".into(), script, root: true, input: None, reload, guard: None, secs: 0, calm: false };
        // 旧版本装在登录用户 crontab 里的任务:迁到 root 之后,把那一行从用户 crontab 里删掉
        let migrate = line.map(|l| sysops::crontab_replace(&info.user_text, Some(l), None));
        self.dock.sys.cron_form = None;
        self.dock.sys.cron_row = None;
        self.sys_run(job, cx);
        if let Some(text) = migrate {
            self.cron_write("迁移旧备份任务", text, cx);
        }
    }

    pub(super) fn cron_write(&mut self, label: &str, text: String, cx: &mut Context<Self>) {
        self.dock.sys.cron_row = None;
        let j = Job { label: label.into(), target: "crontab".into(), script: "crontab -".into(), root: false, input: Some(text), reload: Some((Kind::Cron, sysops::CRON_SCRIPT.to_string(), false)), guard: None, secs: 0, calm: false };
        self.sys_run(j, cx);
    }

    fn cron_pick_inst(&mut self, name: String, cx: &mut Context<Self>) {
        if self.dock.sys.cf.inst.as_deref() == Some(&name) {
            return;
        }
        let cf = &mut self.dock.sys.cf;
        cf.inst = Some(name);
        cf.all = None;
        cf.loading = None;
        cf.dbs.clear();
        cf.skipped = 0;
        cx.notify();
    }

    /// 新增/编辑计划任务的窗口(没打开时是 None)
    pub(crate) fn cron_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if self.page != Page::Cron {
            return None;
        }
        let line = self.dock.sys.cron_form?;
        let ins = self.dock.sys.cron_inputs.clone()?;
        let cf = &self.dock.sys;
        let cf = &cf.cf;
        let editing = line.is_some() || cf.file.is_some();
        let insts = self.cron_insts();
        let sched = self.cron_sched(cx);
        let backup = cf.ty == TaskType::Backup;
        let inst = self.cron_inst();

        // 能不能保存 + 底部那句话
        let (can, summary): (bool, String) = if matches!(cf.ty, TaskType::Prune) {
            match (&sched, cf.prune.any()) {
                (Err(e), _) => (false, e.clone()),
                (_, false) => (false, "至少选一项要清理的内容".into()),
                (Ok(s), _) => (true, format!("{},清理{}", sysops::describe(s), cf.prune.describe())),
            }
        } else if cf.ty == TaskType::Cert {
            match &sched {
                Ok(s) => (true, format!("{},检查网站证书,不到 {} 天到期的自动续期", sysops::describe(s), crate::web::RENEW_DAYS)),
                Err(e) => (false, e.clone()),
            }
        } else if backup {
            if inst.is_none() {
                (false, if insts.is_empty() { "这台服务器上没有找到 PostgreSQL、MySQL、MariaDB 容器".into() } else { "先选一个数据库实例".into() })
            } else if cf.dbs.is_empty() {
                (false, "至少选一个要备份的库".into())
            } else {
                match &sched {
                    Ok(s) => (true, format!("{},备份 {} 个库,保留 {} 份", sysops::describe(s), cf.dbs.len(), cf.keep)),
                    Err(e) => (false, e.clone()),
                }
            }
        } else {
            match &sched {
                Ok(s) if ins[1].read(cx).value().trim().is_empty() => (false, format!("含义:{}", sysops::describe(s))),
                Ok(s) => (true, format!("含义:{}", sysops::describe(s))),
                Err(e) => (false, e.clone()),
            }
        };
        let warn = !can && !summary.is_empty() && !summary.starts_with("含义");

        // ---- 任务类型 ----
        let types: Vec<(&str, TaskType)> = if cf.ty == TaskType::Cert { vec![("证书续期", TaskType::Cert)] } else { vec![("备份数据库", TaskType::Backup), ("Docker 清理", TaskType::Prune), ("自定义命令", TaskType::Custom)] };
        let ty_row = div().flex().flex_row().gap(px(6.0)).children(types.into_iter().enumerate().map(|(i, (label, ty))| {
            chip(("cron-ty", i), label, cf.ty == ty).on_click(cx.listener(move |this, _, _, cx| {
                this.dock.sys.cf.ty = ty;
                if ty == TaskType::Backup && this.dock.sys.cf.inst.is_none() {
                    this.dock.sys.cf.inst = this.cron_insts().first().map(|d| d.container.name.clone());
                }
                cx.notify();
            }))
        }));

        // ---- 类型专属 ----
        let mut sections: Vec<AnyElement> = Vec::new();
        if backup {
            let inst_row = if insts.is_empty() {
                div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("没有找到可备份的数据库容器(支持 PostgreSQL、MySQL、MariaDB)").into_any_element()
            } else {
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(6.0))
                    .children(insts.iter().enumerate().map(|(i, d)| {
                        let name = d.container.name.clone();
                        let on = cf.inst.as_deref() == Some(&d.container.name);
                        chip(("cron-inst", i), format!("{} · {}", d.container.name, d.kind.label()), on).on_click(cx.listener(move |this, _, _, cx| this.cron_pick_inst(name.clone(), cx)))
                    }))
                    .into_any_element()
            };
            sections.push(labeled("数据库实例", "", inst_row).into_any_element());

            let dbs_body: AnyElement = match (&cf.all, &inst) {
                (_, None) => div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("先选实例").into_any_element(),
                (None, _) => div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("正在读取库列表…").into_any_element(),
                (Some(Err(e)), _) => div().text_size(px(t::TYPE_CONTROL)).text_color(c::warn()).child(SharedString::from(e.clone())).into_any_element(),
                (Some(Ok(all)), _) if all.is_empty() => div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("这个实例里还没有可备份的库").into_any_element(),
                (Some(Ok(all)), _) => {
                    let all_on = all.iter().all(|d| cf.dbs.contains(d));
                    let every = all.clone();
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.0))
                        .child(chip(("cron-db-all", 0), if all_on { "取消全选" } else { "全选" }, false).on_click(cx.listener(move |this, _, _, cx| {
                            let cf = &mut this.dock.sys.cf;
                            cf.dbs = if every.iter().all(|d| cf.dbs.contains(d)) { Vec::new() } else { every.clone() };
                            cx.notify();
                        })))
                        .children(all.iter().enumerate().map(|(i, d)| {
                            let name = d.clone();
                            chip(("cron-db", i), d.clone(), cf.dbs.contains(d)).on_click(cx.listener(move |this, _, _, cx| {
                                let cf = &mut this.dock.sys.cf;
                                match cf.dbs.iter().position(|x| *x == name) {
                                    Some(p) => {
                                        cf.dbs.remove(p);
                                    }
                                    None => cf.dbs.push(name.clone()),
                                }
                                cx.notify();
                            }))
                        }))
                        .into_any_element()
                }
            };
            let skipped = cf.skipped;
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .child(labeled("备份哪些库", "", dbs_body))
                    .children((skipped > 0).then(|| div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(SharedString::from(format!("另有 {skipped} 个库的名字含特殊字符(如 -),不能定时备份")))))
                    .into_any_element(),
            );

            let keep_row = div().flex().flex_row().items_center().gap(px(6.0)).children(KEEPS.iter().copied().chain((!KEEPS.contains(&cf.keep)).then_some(cf.keep)).enumerate().map(|(i, n)| {
                chip(("cron-keep", i), format!("{n} 份"), cf.keep == n).on_click(cx.listener(move |this, _, _, cx| {
                    this.dock.sys.cf.keep = n;
                    cx.notify();
                }))
            }));
            sections.push(labeled("保留份数", "只清理这个定时任务自己产生的旧备份,手动备份不会被删;备份存在服务器的备份目录,可在「数据库」页下载、恢复", keep_row).into_any_element());
        } else if cf.ty == TaskType::Prune {
            let o = cf.prune;
            let opts: [(&str, bool, fn(&mut PruneOpts)); 5] = [
                ("悬空镜像", o.dangling, |o| o.dangling = !o.dangling),
                ("已停止的容器", o.containers, |o| o.containers = !o.containers),
                ("未使用的网络", o.networks, |o| o.networks = !o.networks),
                ("构建缓存", o.builder, |o| o.builder = !o.builder),
                ("所有未使用的镜像", o.all_images, |o| o.all_images = !o.all_images),
            ];
            let what = div().flex().flex_row().flex_wrap().gap(px(6.0)).children(opts.into_iter().enumerate().map(|(i, (label, on, f))| {
                chip(("cron-prune", i), label, on).on_click(cx.listener(move |this, _, _, cx| {
                    f(&mut this.dock.sys.cf.prune);
                    cx.notify();
                }))
            }));
            sections.push(labeled("清理什么", "不清理存储卷:里面是数据,请到「存储卷」页手动处理。「所有未使用的镜像」比「悬空镜像」狠,下次启动要重新拉取", what).into_any_element());
            let days = div().flex().flex_row().gap(px(6.0)).children(PRUNE_DAYS.iter().copied().enumerate().map(|(i, n)| {
                chip(("cron-pdays", i), if n == 0 { "不限".to_string() } else { format!("{n} 天") }, o.days == n).on_click(cx.listener(move |this, _, _, cx| {
                    this.dock.sys.cf.prune.days = n;
                    cx.notify();
                }))
            }));
            sections.push(labeled("只清理多久以前的", "超过这个时间没动过的才清理,刚更新留下的旧镜像先保留几天,方便回退", days).into_any_element());
        } else if cf.ty == TaskType::Cert {
            sections.push(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(format!("网站模块装的任务:每天检查一次所有证书,不到 {} 天到期的自动续期。这里只能改检查的时间。", crate::web::RENEW_DAYS)).into_any_element());
        } else {
            sections.push(field("命令", "用登录用户的身份执行,路径写绝对路径更稳妥", &ins[1]).into_any_element());
        }

        // ---- 运行时间 ----
        let time_input = |label: &'static str| {
            div().flex().flex_row().items_center().gap(px(8.0)).child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(label)).child(
                div().w(px(88.0)).h(px(32.0)).flex().items_center().rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border()).text_size(px(t::TYPE_CONTROL)).child(div().flex_1().min_w_0().child(Input::new(&ins[2]).appearance(false).with_size(px(12.0 / 0.875)))),
            )
        };
        let row = || div().flex().flex_row().flex_wrap().items_center().gap(px(6.0));
        let when: AnyElement = if cf.ty == TaskType::Cert {
            time_input("每天").into_any_element()
        } else if cf.advanced {
            field("", "分 时 日 月 周,或 @daily、@reboot 这样的简写", &ins[0]).into_any_element()
        } else {
            let f = cf.freq;
            let kinds = row().children(FREQS.iter().enumerate().map(|(i, (label, k))| {
                let k = *k;
                chip(("cron-freq", i), *label, f.kind == k).on_click(cx.listener(move |this, _, _, cx| {
                    this.dock.sys.cf.freq.kind = k;
                    cx.notify();
                }))
            }));
            let detail: Option<AnyElement> = match f.kind {
                FreqKind::Minutes => Some(
                    row()
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("每隔"))
                        .children(MIN_EVERY.iter().copied().chain((!MIN_EVERY.contains(&f.every) && f.every <= 59).then_some(f.every)).enumerate().map(|(i, n)| {
                            chip(("cron-min", i), n.to_string(), f.every == n).on_click(cx.listener(move |this, _, _, cx| {
                                this.dock.sys.cf.freq.every = n;
                                cx.notify();
                            }))
                        }))
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("分钟"))
                        .into_any_element(),
                ),
                FreqKind::Hours => Some(
                    row()
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("每隔"))
                        .children(HOUR_EVERY.iter().copied().chain((!HOUR_EVERY.contains(&f.every) && f.every <= 23).then_some(f.every)).enumerate().map(|(i, n)| {
                            chip(("cron-hour", i), n.to_string(), f.every == n).on_click(cx.listener(move |this, _, _, cx| {
                                this.dock.sys.cf.freq.every = n;
                                cx.notify();
                            }))
                        }))
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("小时"))
                        .into_any_element(),
                ),
                FreqKind::Daily => Some(time_input("时间").into_any_element()),
                FreqKind::Weekly => Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(row().children(WEEK.iter().enumerate().map(|(i, (label, d))| {
                            let d = *d;
                            chip(("cron-wd", i), *label, f.weekdays & (1 << d) != 0).on_click(cx.listener(move |this, _, _, cx| {
                                this.dock.sys.cf.freq.weekdays ^= 1 << d;
                                cx.notify();
                            }))
                        })))
                        .child(time_input("时间"))
                        .into_any_element(),
                ),
                FreqKind::Monthly => Some(
                    row()
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("每月"))
                        .child(div().w(px(56.0)).h(px(32.0)).flex().items_center().rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border()).text_size(px(t::TYPE_CONTROL)).child(div().flex_1().min_w_0().child(Input::new(&ins[3]).appearance(false).with_size(px(12.0 / 0.875)))))
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("号"))
                        .child(div().w(px(8.0)))
                        .child(time_input("时间"))
                        .into_any_element(),
                ),
                FreqKind::Reboot => None,
            };
            div().flex().flex_col().gap(px(10.0)).child(kinds).children(detail).into_any_element()
        };
        // 「高级」切换:切到高级时把当前选择翻成表达式放进输入框;切回时能反解就回填
        let toggle = div()
            .id("cron-adv")
            .cursor_pointer()
            .text_size(px(t::TYPE_MICRO))
            .text_color(c::ghost())
            .hover(|s| s.text_color(c::secondary()))
            .child(if cf.advanced { "返回简单设置" } else { "高级:直接写 cron" })
            .on_click(cx.listener(|this, _, window, cx| {
                let cur = this.cron_sched(cx).ok();
                let Some(ins) = this.dock.sys.cron_inputs.clone() else { return };
                if this.dock.sys.cf.advanced {
                    let s = ins[0].read(cx).value().to_string();
                    if let Some(f) = Freq::from_cron(&s) {
                        this.dock.sys.cf.freq = f;
                        ins[2].update(cx, |i, cx| i.set_value(&hhmm(f.hour, f.minute), window, cx));
                        ins[3].update(cx, |i, cx| i.set_value(&f.day.to_string(), window, cx));
                    }
                    this.dock.sys.cf.advanced = false;
                } else {
                    if let Some(s) = cur {
                        ins[0].update(cx, |i, cx| i.set_value(&s, window, cx));
                    }
                    this.dock.sys.cf.advanced = true;
                }
                cx.notify();
            }));
        sections.push(
            div()
                .flex()
                .flex_col()
                .gap(px(5.0))
                .child(div().flex().flex_row().items_center().justify_between().child(div().text_size(px(t::TYPE_CAPTION)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child("运行时间")).children((cf.ty != TaskType::Cert).then_some(toggle)))
                .child(when)
                .into_any_element(),
        );

        let title = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(14.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child(if editing { "编辑计划任务" } else { "新增计划任务" }))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(if cf.ty == TaskType::Custom { "写进当前登录用户的 crontab,注释和其他任务保持不变" } else { "在服务器上定时运行,App 关着也会照常执行" }));
        let body = div()
            .id("cron-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .px(px(24.0))
            .pb(px(20.0))
            .child(labeled("任务类型", "", ty_row))
            .children(sections);
        let cancel = crate::node_form::ctl_btn("cron-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
            this.dock.sys.cron_form = None;
            cx.notify();
        }));
        let submit = div()
            .id("cron-ok")
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
            .child("保存")
            .on_click(cx.listener(|this, _, _, cx| this.cron_submit(cx)));
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .child(div().flex_1().min_w_0().text_size(px(t::TYPE_MICRO)).text_color(if warn { c::warn() } else { c::ghost() }).child(SharedString::from(summary)))
            .child(cancel)
            .child(submit);
        Some(crate::ui::modal(
            "cron-dialog",
            560.0,
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    this.dock.sys.cron_form = None;
                    cx.notify();
                    cx.stop_propagation();
                }
                "enter" => {
                    this.cron_submit(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [title.into_any_element(), body.into_any_element(), footer.into_any_element()],
        ))
    }
}
