//! 计划任务里「Docker 清理」类型和网站证书续期的接管的纯逻辑:任务标记的生成与解析、服务器上的脚本、安装脚本。
//! 表单和列表在 `pages/cronform.rs`、`pages/syspages.rs`;设计见 `docs/modules/crontasks.md`。
//!
//! 做法和定时备份(`dbinst.rs`)一致:任务一律 root,脚本放 `/opt/otter/docker/bin/`,计划写 `/etc/cron.d/otter-<类型>-<编号>`,
//! cron 行尾带标记 `# otter:<类型> id=<编号> ...` 记录选项,列表据此显示、编辑据此回填。App 不在也能跑。

use crate::dbinst::valid_task_id;
use crate::srvdata;

pub const BIN: &str = "/opt/otter/docker/bin";
pub const LOGS: &str = "/opt/otter/docker/logs";
pub const PRUNE_MARK: &str = "# otter:docker-prune";
pub const PRUNE_CROND: &str = "/etc/cron.d/otter-docker-prune-";

// ---- Docker 清理 -----------------------------------------------------------------

/// 清理哪些东西
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PruneOpts {
    /// 悬空镜像(没有标签的旧镜像)
    pub dangling: bool,
    pub containers: bool,
    pub networks: bool,
    pub builder: bool,
    /// 所有没被容器使用的镜像
    pub all_images: bool,
    /// 只清理超过 N 天的;0 表示不限
    pub days: u32,
}

impl Default for PruneOpts {
    fn default() -> Self {
        Self { dangling: true, containers: false, networks: false, builder: false, all_images: false, days: 7 }
    }
}

impl PruneOpts {
    fn names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        for (on, n) in [(self.dangling, "dangling"), (self.containers, "containers"), (self.networks, "networks"), (self.builder, "builder"), (self.all_images, "images")] {
            if on {
                v.push(n);
            }
        }
        v
    }

    pub fn any(&self) -> bool {
        !self.names().is_empty()
    }

    /// 一句话:「悬空镜像、已停止的容器,只清理超过 7 天的」
    pub fn describe(&self) -> String {
        let mut v = Vec::new();
        for (on, n) in [(self.all_images, "所有未使用的镜像"), (self.dangling && !self.all_images, "悬空镜像"), (self.containers, "已停止的容器"), (self.networks, "未使用的网络"), (self.builder, "构建缓存")] {
            if on {
                v.push(n);
            }
        }
        let mut s = v.join("、");
        if self.days > 0 {
            s.push_str(&format!(",只清理超过 {} 天的", self.days));
        }
        s
    }
}

/// 任务的 cron 命令部分:`<脚本> # otter:docker-prune id=<编号> opts=a,b days=7`
pub fn prune_cmd(bin: &str, id: &str, o: &PruneOpts) -> String {
    format!("{bin}/prune-{id}.sh {PRUNE_MARK} id={id} opts={} days={}", o.names().join(","), o.days)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneTask {
    pub id: String,
    pub opts: PruneOpts,
    pub script: String,
}

pub fn parse_prune(cmd: &str) -> Option<PruneTask> {
    let (path, rest) = cmd.split_once(PRUNE_MARK)?;
    let script = path.trim().to_string();
    let (mut id, mut o) = (String::new(), PruneOpts { dangling: false, days: 0, ..Default::default() });
    for tok in rest.split_whitespace() {
        if let Some(v) = tok.strip_prefix("id=") {
            id = v.to_string();
        } else if let Some(v) = tok.strip_prefix("opts=") {
            for n in v.split(',') {
                match n {
                    "dangling" => o.dangling = true,
                    "containers" => o.containers = true,
                    "networks" => o.networks = true,
                    "builder" => o.builder = true,
                    "images" => o.all_images = true,
                    _ => {}
                }
            }
        } else if let Some(v) = tok.strip_prefix("days=") {
            o.days = v.parse().unwrap_or(0);
        }
    }
    valid_task_id(&id).then_some(PruneTask { id, opts: o, script })
}

pub fn prune_crond(id: &str) -> String {
    format!("{PRUNE_CROND}{id}")
}

/// 选 docker 的写法:能直接用就直接用,否则 `sudo -n`(root 跑时就是前者)
const DOCKER_PRELUDE: &str = concat!(
    "#!/bin/sh\n",
    "PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin; export PATH\n",
    "DS=; if docker info >/dev/null 2>&1; then docker() { command docker \"$@\"; }; ",
    "elif sudo -n docker info >/dev/null 2>&1; then DS='sudo -n'; docker() { sudo -n docker \"$@\"; }; ",
    "else echo \"没有 docker 权限\" >&2; exit 1; fi\n",
);

fn log_prelude(file: &str) -> String {
    format!("mkdir -p {LOGS}; LOGF={LOGS}/{file}; log() {{ printf '%s %s\\n' \"$(date '+%F %T')\" \"$1\" >>\"$LOGF\"; }}\n")
}

const LOG_TRIM: &str = "[ -f \"$LOGF\" ] && tail -n 200 \"$LOGF\" >\"$LOGF.t\" && mv \"$LOGF.t\" \"$LOGF\"\n";

/// 清理脚本:每一项一条命令,结果(释放的空间或失败原因)写日志
pub fn prune_script(o: &PruneOpts) -> Result<String, String> {
    if !o.any() {
        return Err("至少选一项要清理的内容".into());
    }
    if o.days > 3650 {
        return Err("天数不合理".into());
    }
    let mut s = String::from(DOCKER_PRELUDE);
    s.push_str("# Otter 定时清理 Docker:由 Otter 生成,请在 Otter 的「计划任务」里修改,手改会被覆盖\n");
    s.push_str(&log_prelude("prune.log"));
    let until = if o.days > 0 { format!(" --filter until={}h", o.days * 24) } else { String::new() };
    let mut step = |name: &str, cmd: String| {
        s.push_str(&format!("out=$(docker {cmd} 2>&1) && log \"{name}: $(printf '%s' \"$out\" | tail -n1)\" || log \"{name}失败: $(printf '%s' \"$out\" | tail -n1)\"\n"));
    };
    if o.containers {
        step("已停止的容器", format!("container prune -f{until}"));
    }
    if o.all_images {
        step("未使用的镜像", format!("image prune -a -f{until}"));
    } else if o.dangling {
        step("悬空镜像", format!("image prune -f{until}"));
    }
    if o.networks {
        step("未使用的网络", format!("network prune -f{until}"));
    }
    if o.builder {
        step("构建缓存", format!("builder prune -f{until}"));
    }
    s.push_str(LOG_TRIM);
    s.push_str("exit 0\n");
    Ok(s)
}

// ---- 安装 ------------------------------------------------------------------------

/// 装一个任务(root 身份):脚本写进 `/opt/otter/docker/bin/<name>`,计划写进 `file`。
/// `cmd` 里的 `@@BIN@@` 会被换成脚本目录
pub fn install(name: &str, body: &str, file: &str, sched: &str, cmd: &str) -> Result<String, String> {
    let ok_name = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'));
    if !ok_name || body.contains("OTTER_EOF") || !crate::sysops::valid_schedule(sched) || cmd.contains(['\n', '\r']) || file.contains(['\'', '"', ' ', '\n']) {
        return Err("任务名或计划里有不支持的字符".into());
    }
    Ok(format!(
        r#"B={BIN}; mkdir -p "$B" {LOGS} || exit 1; cat >"$B/{name}" <<'OTTER_EOF'
{body}OTTER_EOF
chmod 700 "$B/{name}" || exit 1; sed "s#@@BIN@@#$B#g" >"{file}" <<'OTTER_EOF'
{sched} root {cmd}
OTTER_EOF
chmod 644 "{file}"
"#
    ))
}

// ---- 证书续期(网站模块装的,只接管显示和改时间)-----------------------------------

/// 计划任务页里认出网站模块的续期任务
pub fn is_cert_renew(source: &str, cmd: &str) -> bool {
    source == crate::web::CRON_FILE && cmd.trim() == crate::web::RENEW_SH
}

/// 续期日志在服务器上的位置
pub fn renew_log() -> String {
    format!("{}/logs/renew.log", srvdata::WEB)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_round_trip() {
        let o = PruneOpts { dangling: true, containers: true, days: 3, ..Default::default() };
        let cmd = prune_cmd("/opt/otter/docker/bin", "ab12cd", &o);
        let t = parse_prune(&cmd).unwrap();
        assert_eq!((t.id.as_str(), t.opts), ("ab12cd", o));
        assert!(parse_prune("/x.sh # otter:docker-prune id=x opts=images").is_none());
        assert_eq!(PruneOpts::default().describe(), "悬空镜像,只清理超过 7 天的");
    }

    fn sh_ok(script: &str) {
        let dir = std::env::temp_dir().join(format!("otter-ct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(format!("{}.sh", script.len()));
        std::fs::write(&f, script).unwrap();
        let out = std::process::Command::new("sh").arg("-n").arg(&f).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scripts_are_valid_sh() {
        sh_ok(&prune_script(&PruneOpts { dangling: true, containers: true, networks: true, builder: true, all_images: false, days: 7 }).unwrap());
        sh_ok(&prune_script(&PruneOpts { all_images: true, days: 0, ..Default::default() }).unwrap());
        assert!(prune_script(&PruneOpts { dangling: false, ..Default::default() }).is_err());
    }

    #[test]
    fn install_checks() {
        assert!(install("prune-ab12cd.sh", "echo\n", "/etc/cron.d/otter-docker-prune-ab12cd", "0 3 * * *", "@@BIN@@/prune-ab12cd.sh").is_ok());
        assert!(install("a;b", "x", "/etc/cron.d/x", "0 3 * * *", "c").is_err());
    }
}
