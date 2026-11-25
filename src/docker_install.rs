//! 一键安装 Docker:探测服务器、测速选源、生成安装脚本。
//!
//! 只有纯函数(脚本文本 + 解析 + 选择规则),执行和进度在 `docker.rs`。设计见 docs/plans/active/2026-11-25-docker-install.md。
//! 做法参考 1Panel:`DOWNLOAD_URL` 指向 docker-ce 源 + Docker 官方 `get-docker.sh`;
//! 不同的是地区不靠 IP 归属地判断,而是在服务器上对各个源实测,官方源优先。

use std::collections::HashMap;

/// 一个 docker-ce 软件源
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Official,
    Aliyun,
    Tencent,
    Netease,
    Cernet,
}

impl Source {
    pub const ALL: [Source; 5] = [Source::Official, Source::Aliyun, Source::Tencent, Source::Netease, Source::Cernet];

    pub fn label(self) -> &'static str {
        match self {
            Source::Official => "官方",
            Source::Aliyun => "阿里云",
            Source::Tencent => "腾讯云",
            Source::Netease => "网易",
            Source::Cernet => "中科大",
        }
    }

    /// `DOWNLOAD_URL` 的值(官方脚本认这个环境变量)
    pub fn base(self) -> &'static str {
        match self {
            Source::Official => "https://download.docker.com",
            Source::Aliyun => "https://mirrors.aliyun.com/docker-ce",
            Source::Tencent => "https://mirrors.tencent.com/docker-ce",
            Source::Netease => "https://mirrors.163.com/docker-ce",
            Source::Cernet => "https://mirrors.cernet.edu.cn/docker-ce",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Source::Official => "official",
            Source::Aliyun => "aliyun",
            Source::Tencent => "tencent",
            Source::Netease => "netease",
            Source::Cernet => "cernet",
        }
    }
}

/// Docker Hub 镜像加速地址(同 1Panel)
pub const REGISTRY_MIRRORS: [&str; 3] = ["https://docker.1panel.live", "https://hub.1panel.dev", "https://docker.1ms.run"];

/// Docker Hub 慢到这个秒数以上(或不通),就配置镜像加速
const HUB_SLOW: f32 = 1.5;
/// 官方源耗时不超过最快镜像的这个倍数,仍选官方
const OFFICIAL_SLACK: f32 = 2.0;

/// 探测结果
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    pub kernel: String,
    pub os_id: String,
    pub os_name: String,
    pub arch: String,
    pub root: bool,
    /// 免密 sudo 可用
    pub sudo_n: bool,
    /// 能拿到 root:本来就是 root、免密 sudo,或节点保存了密码(走 `sudo -S`)
    pub can_root: bool,
    pub curl: bool,
    pub wget: bool,
    /// 服务器上已经有一次安装在进行(上次连接断了、Otter 重开后接着看)
    pub inst_running: bool,
    pub systemd: bool,
    /// 各源下载小文件的耗时(秒);不通是 None
    pub speeds: Vec<(Source, Option<f32>)>,
    /// registry-1.docker.io 的耗时;不通是 None
    pub hub: Option<f32>,
}

/// 探测脚本:只读,不需要 root。各源并行测 2 次取平均,单次超时 3 秒。
pub const PROBE_SCRIPT: &str = r#"
[ -r /etc/os-release ] && . /etc/os-release
D=/var/tmp/otter-docker-install
pid=$(cat $D/pid 2>/dev/null)
echo "inst_running=$([ -n "$pid" ] && [ ! -f $D/status ] && ps -eo pgid= 2>/dev/null | awk -v g="$pid" '$1==g{f=1} END{exit !f}' && echo 1 || echo 0)"
echo "kernel=$(uname -s)"
echo "os_id=${ID:-unknown}"
echo "os_name=${PRETTY_NAME:-${NAME:-unknown}}"
echo "arch=$(uname -m)"
echo "root=$([ "$(id -u)" = 0 ] && echo 1 || echo 0)"
echo "sudo=$(sudo -n true >/dev/null 2>&1 && echo 1 || echo 0)"
echo "curl=$(command -v curl >/dev/null 2>&1 && echo 1 || echo 0)"
echo "wget=$(command -v wget >/dev/null 2>&1 && echo 1 || echo 0)"
echo "systemd=$([ -d /run/systemd/system ] && echo 1 || echo 0)"
command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 || exit 0
d=$(mktemp -d 2>/dev/null || echo /tmp)
now() { n=$(date +%s.%N 2>/dev/null); case "$n" in ""|*N*) n=$(date +%s);; esac; echo "$n"; }
# 一次请求的耗时(秒);没有 curl 时用 wget 加 date 计时。连不上返回非 0
one() {
  if command -v curl >/dev/null 2>&1; then
    curl -o /dev/null -s -m 3 -w '%{time_total}' "$1" 2>/dev/null
  else
    a=$(now); wget -q -T 3 -t 1 -O /dev/null "$1" 2>/dev/null; rc=$?
    # 8 是服务器回了 4xx/5xx(比如 registry 的 /v2/ 返回 401),说明连得上
    [ "$rc" = 0 ] || [ "$rc" = 8 ] || return 1
    b=$(now); awk "BEGIN{print $b-$a}"
  fi
}
t() {
  tot=0; ok=1
  for _ in 1 2; do
    s=$(one "$2") || { ok=0; break; }
    tot=$(awk "BEGIN{print $tot+$s}")
  done
  if [ "$ok" = 1 ]; then awk "BEGIN{printf \"speed.$1=%.3f\n\", $tot/2}"; else echo "speed.$1=fail"; fi
}
t official https://download.docker.com/linux/debian/gpg > "$d/official" &
t aliyun https://mirrors.aliyun.com/docker-ce/linux/debian/gpg > "$d/aliyun" &
t tencent https://mirrors.tencent.com/docker-ce/linux/debian/gpg > "$d/tencent" &
t netease https://mirrors.163.com/docker-ce/linux/debian/gpg > "$d/netease" &
t cernet https://mirrors.cernet.edu.cn/docker-ce/linux/debian/gpg > "$d/cernet" &
t hub https://registry-1.docker.io/v2/ > "$d/hub" &
wait
cat "$d"/official "$d"/aliyun "$d"/tencent "$d"/netease "$d"/cernet "$d"/hub 2>/dev/null
rm -rf "$d" 2>/dev/null
"#;

pub fn parse_probe(out: &str, has_password: bool) -> Probe {
    let kv: HashMap<&str, &str> = out.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim(), v.trim())).collect();
    let speed = |k: &str| kv.get(format!("speed.{k}").as_str()).and_then(|v| v.parse::<f32>().ok());
    let (root, sudo_n) = (kv.get("root") == Some(&"1"), kv.get("sudo") == Some(&"1"));
    Probe {
        root,
        sudo_n,
        can_root: root || sudo_n || has_password,
        kernel: kv.get("kernel").unwrap_or(&"").to_string(),
        os_id: kv.get("os_id").unwrap_or(&"").to_string(),
        os_name: kv.get("os_name").unwrap_or(&"").to_string(),
        arch: kv.get("arch").unwrap_or(&"").to_string(),
        curl: kv.get("curl") == Some(&"1"),
        wget: kv.get("wget") == Some(&"1"),
        inst_running: kv.get("inst_running") == Some(&"1"),
        systemd: kv.get("systemd") == Some(&"1"),
        speeds: Source::ALL.iter().map(|s| (*s, speed(s.key()))).collect(),
        hub: speed("hub"),
    }
}

impl Probe {
    /// 不能装时的原因
    pub fn blocked(&self) -> Option<String> {
        if self.os_id.is_empty() {
            return Some("读不到系统信息(/etc/os-release)".into());
        }
        if !self.kernel.is_empty() && self.kernel != "Linux" {
            return Some(format!("一键安装只支持 Linux 服务器(这台是 {})", self.kernel));
        }
        if !self.can_root {
            return Some("需要 root 权限:当前用户不是 root,sudo 也需要密码。请在节点里保存登录密码,或改用 root 登录".into());
        }
        if !self.curl && !self.wget {
            return Some("服务器上既没有 curl 也没有 wget,请先安装其中一个".into());
        }
        if !matches!(self.arch.as_str(), "x86_64" | "aarch64" | "armv7l" | "s390x" | "ppc64le") {
            return Some(format!("暂不支持 {} 架构", self.arch));
        }
        if self.speeds.iter().all(|(_, s)| s.is_none()) {
            return Some("服务器连不上任何 Docker 安装源,请检查网络".into());
        }
        None
    }

    /// 自动选源:官方可达且不比最快的镜像慢太多就用官方,否则用最快的镜像
    pub fn pick(&self) -> Option<Source> {
        let official = self.speeds.iter().find(|(s, _)| *s == Source::Official).and_then(|(_, v)| *v);
        let best_mirror = self.speeds.iter().filter(|(s, _)| *s != Source::Official).filter_map(|(s, v)| v.map(|v| (*s, v))).min_by(|a, b| a.1.total_cmp(&b.1));
        match (official, best_mirror) {
            (Some(o), Some((_, m))) if o <= m * OFFICIAL_SLACK => Some(Source::Official),
            (Some(_), None) => Some(Source::Official),
            (_, Some((s, _))) => Some(s),
            (None, None) => None,
        }
    }

    /// 安装时尝试源的顺序:先是 `first`,再是其余能连通的源,按速度从快到慢;最多 4 个
    pub fn ranked(&self, first: Source) -> Vec<Source> {
        let mut rest: Vec<(Source, f32)> = self.speeds.iter().filter(|(s, _)| *s != first).filter_map(|(s, v)| v.map(|v| (*s, v))).collect();
        rest.sort_by(|a, b| a.1.total_cmp(&b.1));
        std::iter::once(first).chain(rest.into_iter().map(|(s, _)| s)).take(4).collect()
    }

    pub fn speed_of(&self, s: Source) -> Option<f32> {
        self.speeds.iter().find(|(x, _)| *x == s).and_then(|(_, v)| *v)
    }

    /// Docker Hub 慢或不通:建议配置镜像加速
    pub fn hub_slow(&self) -> bool {
        self.hub.is_none_or(|h| h > HUB_SLOW)
    }
}

/// 由日志推算安装进度:(0~1 的比例, 当前步骤)。没有真实百分比可取,这是按阶段估的:
/// 环境检查 → 下载官方脚本 → 官方脚本里的各步(更新软件源、加源、装包、配置、验证)→ 启动 → 收尾。
pub fn progress(log: &[String]) -> (f32, &'static str) {
    let mut best = (0.04f32, "准备安装");
    let mut up = |v: f32, label: &'static str| {
        if v >= best.0 {
            best = (v, label);
        }
    };
    // 官方脚本的细分步骤只看最后一次尝试(换源重试会从头来)
    let attempt = log.iter().rposition(|l| l.starts_with("==> 安装 Docker(源"));
    let start = attempt.unwrap_or(usize::MAX);
    for (i, l) in log.iter().enumerate() {
        // 等待锁时列出的占用进程(「占用:」开头)不是这次安装自己的步骤
        if l.trim_start().starts_with("占用:") {
            continue;
        }
        let low = l.to_lowercase();
        if l.contains("包管理进程") {
            up(0.06, "等待其他包管理进程结束");
        } else if l.starts_with("==> 下载 Docker 官方安装脚本") {
            up(0.12, "下载官方安装脚本");
        } else if l.starts_with("==> 安装 Docker(源") {
            up(0.22, "开始安装");
        } else if l.starts_with("==> 启动 Docker") {
            up(0.92, "启动 Docker");
        } else if l.starts_with("==> 配置 Docker Hub") {
            up(0.96, "配置镜像加速");
        } else if l.starts_with("==> Docker 已安装") {
            up(0.99, "即将完成");
        } else if i >= start {
            if (low.contains("apt-get") && low.contains("update")) || low.contains("makecache") {
                up(0.32, "更新软件源");
            } else if low.contains("docker.asc") || low.contains("docker.list") || low.contains("docker-ce.repo") || low.contains("add-repo") || low.contains("gpg") {
                up(0.42, "添加 Docker 软件源");
            } else if low.contains("docker-ce") && (low.contains("install") || low.contains("安装")) {
                up(0.55, "下载并安装 Docker 软件包");
            } else if low.starts_with("unpacking") || low.starts_with("setting up") || low.contains("正在解包") || low.contains("正在设置") || low.starts_with("installed:") || low.starts_with("installing") {
                up(0.75, "配置 Docker 软件包");
            } else if low.contains("docker version") || low.starts_with("client:") || low.contains("server: docker engine") {
                up(0.86, "验证安装");
            }
        }
    }
    best
}

/// 测速结果的显示:不到 1 秒用毫秒,否则用秒
pub fn fmt_speed(secs: f32) -> String {
    if secs < 1.0 { format!("{} ms", (secs * 1000.0).round() as u32) } else { format!("{secs:.1} s") }
}

/// 安装脚本(要 root)。官方脚本地址按顺序兜底,第一个能下载的执行;
/// 按 `srcs` 的顺序逐个源尝试,一个源装失败(镜像同步不全、断流等)就自动换下一个;装完启用并启动服务,`docker version` 验证。
pub fn install_script(srcs: &[Source]) -> String {
    let entries = srcs
        .iter()
        .map(|s| format!("\"{}|{}\"", s.label(), if *s == Source::Official { "" } else { s.base() }))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"set -e
d=$(mktemp -d)
trap 'rm -rf "$d"' EXIT
cd "$d"
# 其他包管理进程(自动更新等)占着锁时,apt 会直接失败:最多等 3 分钟
pkgbusy() {{
  if command -v fuser >/dev/null 2>&1; then fuser /var/lib/dpkg/lock-frontend /var/lib/dpkg/lock /var/lib/apt/lists/lock >/dev/null 2>&1
  elif command -v pgrep >/dev/null 2>&1; then pgrep -x apt >/dev/null || pgrep -x apt-get >/dev/null || pgrep -x dpkg >/dev/null || pgrep -f unattended-upgr >/dev/null
  else return 1; fi
}}
if command -v dpkg >/dev/null 2>&1; then
  n=0
  while pkgbusy; do
    if [ "$n" = 0 ]; then
      echo "有其他包管理进程(如系统自动更新、上次被取消的安装)正在运行,等待它结束(最多 3 分钟)…"
      ps -eo pid,etime,args 2>/dev/null | grep -E 'apt|dpkg|unattended-upgr|get-docker' | grep -v grep | head -4 | cut -c1-120 | sed 's/^/  占用: /'
    fi
    n=$((n+1)); [ "$n" -gt 36 ] && {{ echo "其他包管理进程一直占着锁(Could not get lock),请稍后重试" >&2; exit 1; }}
    sleep 5
  done
  # 上次安装被中断(断网、重启)时 dpkg 可能处于半配置状态,先修复
  dpkg --configure -a 2>&1 || true
fi
echo "==> 下载 Docker 官方安装脚本"
dl() {{
  if command -v curl >/dev/null 2>&1; then curl -fsSL --retry 2 --retry-delay 3 --connect-timeout 5 --max-time 20 "$1" -o "$2"
  else wget -q -T 20 -t 2 -O "$2" "$1"; fi
}}
for u in https://get.docker.com \
  https://testingcf.jsdelivr.net/gh/docker/docker-install@master/install.sh \
  https://cdn.jsdelivr.net/gh/docker/docker-install@master/install.sh \
  https://fastly.jsdelivr.net/gh/docker/docker-install@master/install.sh \
  https://gcore.jsdelivr.net/gh/docker/docker-install@master/install.sh \
  https://raw.githubusercontent.com/docker/docker-install/master/install.sh; do
  echo "尝试 $u"
  if dl "$u" get-docker.sh; then break; fi
  rm -f get-docker.sh
done
[ -s get-docker.sh ] || {{ echo "无法下载 Docker 官方安装脚本,请检查服务器网络" >&2; exit 1; }}
# 这台机器还没有 Docker,apt 的 docker.list 要么不存在,要么是上次失败留下的(可能指向坏镜像,会让 get-docker.sh 开头的 apt-get update 就失败):
# 装之前先删掉,官方脚本会按这次的源重新生成;全部失败时也删掉,不留下指向坏镜像的源
apt_list=/etc/apt/sources.list.d/docker.list
rm -f "$apt_list"
# yum 的 docker-ce.repo 装之前没有的失败时删掉,有的恢复原样
repos="/etc/yum.repos.d/docker-ce.repo"
for f in $repos; do b="$d/$(basename "$f")"; if [ -e "$f" ]; then cp -p "$f" "$b.bak"; else : > "$b.none"; fi; done
restore() {{
  rm -f "$apt_list"
  for f in $repos; do b="$d/$(basename "$f")"
    if [ -e "$b.bak" ]; then cp -p "$b.bak" "$f"; elif [ -e "$b.none" ]; then rm -f "$f"; fi
  done
}}
ok=0
for entry in {entries}; do
  label=${{entry%%|*}}; url=${{entry#*|}}
  echo "==> 安装 Docker(源:$label)"
  if [ -n "$url" ]; then export DOWNLOAD_URL="$url"; else unset DOWNLOAD_URL; fi
  {{ rc=0; sh get-docker.sh 2>&1 || rc=$?; echo $rc > "$d/rc"; }} | tee "$d/attempt.log"
  if [ "$(cat "$d/rc")" = 0 ]; then ok=1; break; fi
  # 和源无关的失败(占锁、磁盘满、权限、不支持的系统):换源没用,直接停
  if grep -qiE 'could not get lock|unable to acquire the dpkg|no space left on device|password is required|a terminal is required|unsupported distribution|is not supported|无法获得锁|无法获取锁' "$d/attempt.log"; then
    echo "==> 这个失败和安装源无关,不再换源" >&2
    restore; exit 1
  fi
  echo "==> 源 $label 安装失败,换下一个源重试"
  sleep 3
done
[ "$ok" = 1 ] || {{ echo "所有安装源都失败了" >&2; restore; exit 1; }}
echo "==> 启动 Docker"
if command -v systemctl >/dev/null 2>&1; then systemctl enable docker 2>&1 || true; systemctl daemon-reload || true; systemctl start docker 2>&1 || true; else service docker start 2>&1 || service dockerd start 2>&1 || true; fi
sleep 1
docker version >/dev/null 2>&1 || {{ echo "安装完成但 docker 没有正常运行" >&2; exit 1; }}
echo "==> Docker 已安装"
"#,
    )
}

/// 服务器上放安装脚本、日志、退出码的目录(root 创建,所有人可读,方便非 root 轮询)
pub const INSTALL_DIR: &str = "/var/tmp/otter-docker-install";

/// 启动脚本(要 root):把安装脚本(从 stdin 读)写到服务器上,脱离 SSH 连接在后台运行;
/// 输出追加到 `log`,结束后把退出码写进 `status`。已有安装在进行时不重复启动。
pub fn start_script(accel: bool) -> String {
    let d = INSTALL_DIR;
    let accel_line = if accel { format!("touch {d}/accel") } else { String::new() };
    format!(
        r#"D={d}
if [ -f $D/pid ] && [ ! -f $D/status ] && ps -eo pgid= 2>/dev/null | awk -v g="$(cat $D/pid)" '$1==g{{f=1}} END{{exit !f}}'; then echo "已有一次安装在进行"; exit 0; fi
rm -rf $D; mkdir -p $D; chmod 755 $D
cat > $D/run.sh
{accel_line}
: > $D/log; chmod 644 $D/log
run='echo $$ > {d}/pid; sh {d}/run.sh >> {d}/log 2>&1 < /dev/null; echo $? > {d}/status'
if command -v setsid >/dev/null 2>&1; then setsid nohup sh -c "$run" >/dev/null 2>&1 < /dev/null &
else nohup sh -c "$run" >/dev/null 2>&1 < /dev/null &
fi
sleep 1
"#
    )
}

/// 轮询脚本(不需要 root):状态、进程是否还在、从 `offset` 字节起的新日志
pub fn poll_script(offset: usize) -> String {
    let d = INSTALL_DIR;
    format!(
        r#"D={d}
echo "ST=$(cat $D/status 2>/dev/null)"
pid=$(cat $D/pid 2>/dev/null)
# 进程组里只要还有一个进程(apt-get、dpkg 等)就算还在跑,取消后要等它们真的退出
echo "ALIVE=$([ -n "$pid" ] && ps -eo pgid= 2>/dev/null | awk -v g="$pid" '$1==g{{f=1}} END{{exit !f}}' && echo 1 || echo 0)"
echo "ACCEL=$([ -f $D/accel ] && echo 1 || echo 0)"
echo "ACCEL_DONE=$([ -f $D/accel-done ] && echo 1 || echo 0)"
echo "--LOG--"
tail -c +{} $D/log 2>/dev/null
exit 0
"#,
        offset + 1
    )
}

/// 取消:结束整个安装进程组
pub fn cancel_script() -> String {
    let d = INSTALL_DIR;
    format!("D={d}; pid=$(cat $D/pid 2>/dev/null); [ -n \"$pid\" ] && {{ kill -s TERM -- -$pid 2>/dev/null || kill -s TERM $pid 2>/dev/null; }}; sleep 1; exit 0")
}

/// 安装结束(成功、失败、取消)并且结果已经读完后,删掉服务器上的脚本、日志和状态文件(要 root)
/// `failed` 时(失败、取消)再删掉这次留下的 apt docker.list:Docker 没装上,留着只会让 apt update 指向可能有问题的镜像
pub fn cleanup_script(failed: bool) -> String {
    if failed { format!("rm -rf {INSTALL_DIR}; rm -f /etc/apt/sources.list.d/docker.list") } else { format!("rm -rf {INSTALL_DIR}") }
}

/// 标记镜像加速已配置(要 root)
pub fn accel_done_script() -> String {
    format!("touch {INSTALL_DIR}/accel-done")
}

/// 一次轮询的结果
#[derive(Debug, Default, PartialEq)]
pub struct Poll {
    /// 安装脚本的退出码;还没结束是 None
    pub status: Option<i32>,
    pub alive: bool,
    pub accel: bool,
    pub accel_done: bool,
    /// 这次新读到的日志字节
    pub log: Vec<u8>,
}

pub fn parse_poll(raw: &[u8]) -> Poll {
    const MARK: &[u8] = b"--LOG--\n";
    let at = raw.windows(MARK.len()).position(|w| w == MARK);
    let (head, log) = match at {
        Some(i) => (&raw[..i], raw[i + MARK.len()..].to_vec()),
        None => (raw, Vec::new()),
    };
    let head = String::from_utf8_lossy(head);
    let kv: HashMap<&str, &str> = head.lines().filter_map(|l| l.split_once('=')).collect();
    Poll {
        status: kv.get("ST").and_then(|v| v.trim().parse().ok()),
        alive: kv.get("ALIVE") == Some(&"1"),
        accel: kv.get("ACCEL") == Some(&"1"),
        accel_done: kv.get("ACCEL_DONE") == Some(&"1"),
        log,
    }
}

/// 把日志里常见的失败原因翻成中文提示;认不出来返回 None
pub fn explain(log: &str) -> Option<&'static str> {
    let low = log.to_lowercase();
    let has = |k: &str| low.contains(k);
    if has("could not get lock") || has("unable to acquire the dpkg") || has("lock-frontend") || has("waiting for cache lock") {
        Some("有其他包管理进程在占用(常见于系统正在自动更新),请稍后重试")
    } else if has("no space left on device") {
        Some("服务器磁盘空间不足,请清理后重试")
    } else if has("hashes of expected file") || has("hash sum mismatch") || has("哈希校验和不符") || has("部分索引文件") || has("some index files failed") {
        Some("所选安装源的镜像内容不一致(多半正在同步),请稍后重试,或在安装源里改选另一个")
    } else if has("could not resolve host") || has("temporary failure in name resolution") || has("name or service not known") {
        Some("服务器解析不了域名,请检查服务器的 DNS 设置")
    } else if has("connection timed out") || has("failed to connect") || has("network is unreachable") || has("connection refused") || has("operation timed out") {
        Some("服务器网络不通或太慢,请检查网络后重试")
    } else if has("ssl certificate problem") || has("certificate verification failed") || has("certificate is not yet valid") || has("certificate has expired") {
        Some("证书校验失败,常见原因是服务器时间不对,请先校准时间")
    } else if has("dpkg was interrupted") {
        Some("上次安装被中断了,系统已尝试修复,请重试")
    } else if has("password is required") || has("a terminal is required") || has("not in the sudoers") {
        Some("需要 root 权限:请在节点里保存登录密码,或改用 root 登录")
    } else if has("unsupported distribution") || has("unsupported os") || has("not supported") && has("distribution") {
        Some("官方安装脚本不支持这个发行版")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GLOBAL: &str = "os_id=ubuntu\nos_name=Ubuntu 22.04.4 LTS\narch=x86_64\nroot=0\ncurl=1\nsystemd=1\nspeed.official=0.210\nspeed.aliyun=0.350\nspeed.tencent=0.400\nspeed.netease=fail\nspeed.cernet=0.900\nspeed.hub=0.300\n";
    const CN: &str = "os_id=debian\nos_name=Debian GNU/Linux 12 (bookworm)\narch=aarch64\nroot=1\ncurl=1\nsystemd=1\nspeed.official=fail\nspeed.aliyun=0.040\nspeed.tencent=0.060\nspeed.netease=0.200\nspeed.cernet=0.100\nspeed.hub=fail\n";

    #[test]
    fn overseas_picks_official() {
        let p = parse_probe(GLOBAL, true);
        assert_eq!(p.os_name, "Ubuntu 22.04.4 LTS");
        assert!(!p.root && p.can_root && p.curl && p.systemd);
        assert_eq!(p.blocked(), None);
        assert_eq!(p.pick(), Some(Source::Official));
        assert!(!p.hub_slow());
    }

    #[test]
    fn mainland_picks_fastest_mirror_and_accelerates() {
        let p = parse_probe(CN, false);
        assert_eq!(p.pick(), Some(Source::Aliyun));
        assert!(p.hub_slow());
        assert_eq!(p.speed_of(Source::Official), None);
    }

    #[test]
    fn official_slightly_slower_still_official() {
        let p = parse_probe("os_id=debian\narch=x86_64\ncurl=1\nspeed.official=0.50\nspeed.aliyun=0.30\n", true);
        assert_eq!(p.pick(), Some(Source::Official));
        let p = parse_probe("os_id=debian\narch=x86_64\ncurl=1\nspeed.official=1.50\nspeed.aliyun=0.30\n", true);
        assert_eq!(p.pick(), Some(Source::Aliyun));
    }

    #[test]
    fn blocked_reasons() {
        assert!(parse_probe("", true).blocked().unwrap().contains("系统信息"));
        assert!(parse_probe("os_id=debian\narch=x86_64\ncurl=0\nwget=0\n", true).blocked().unwrap().contains("curl"));
        assert!(parse_probe("os_id=debian\narch=riscv64\ncurl=1\n", true).blocked().unwrap().contains("架构"));
        assert!(parse_probe("os_id=debian\narch=x86_64\ncurl=1\nspeed.official=fail\nspeed.aliyun=fail\n", true).blocked().unwrap().contains("网络"));
    }

    #[test]
    fn wget_is_enough() {
        let p = parse_probe("os_id=debian\narch=x86_64\ncurl=0\nwget=1\nspeed.official=0.5\n", true);
        assert_eq!(p.blocked(), None);
    }

    #[test]
    fn needs_root() {
        let p = parse_probe("os_id=debian\narch=x86_64\ncurl=1\nroot=0\nsudo=0\nspeed.official=0.1\n", false);
        assert!(p.blocked().unwrap().contains("root"));
        assert_eq!(parse_probe("os_id=debian\narch=x86_64\ncurl=1\nroot=0\nsudo=1\nspeed.official=0.1\n", false).blocked(), None);
    }

    #[test]
    fn script_uses_mirror_only_when_not_official() {
        assert!(!install_script(&[Source::Official]).contains("mirrors."));
        assert!(install_script(&[Source::Tencent]).contains("\"腾讯云|https://mirrors.tencent.com/docker-ce\""));
    }

    #[test]
    fn scripts_pass_sh_syntax_check() {
        use std::io::Write;
        for script in [install_script(&[Source::Tencent, Source::Official]), start_script(true), poll_script(0), cancel_script(), PROBE_SCRIPT.to_string()] {
            let mut child = std::process::Command::new("sh").arg("-n").stdin(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
    }

    #[test]
    fn progress_follows_log() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(progress(&[]).1, "准备安装");
        let a = progress(&l(&["==> 下载 Docker 官方安装脚本"]));
        let b = progress(&l(&["==> 下载 Docker 官方安装脚本", "==> 安装 Docker(源:腾讯云)", "+ sh -c apt-get -qq update >/dev/null"]));
        let c = progress(&l(&["==> 安装 Docker(源:腾讯云)", "+ sh -c apt-get -y -qq install docker-ce docker-ce-cli", "Setting up docker-ce (5:27)", "+ sh -c docker version"]));
        let d = progress(&l(&["==> 启动 Docker"]));
        assert!(a.0 < b.0 && b.0 < c.0 && c.0 < d.0 && d.0 < 1.0);
        assert_eq!(b.1, "更新软件源");
        assert_eq!(c.1, "验证安装");
        // 换源重试:回到新一次尝试的开头
        let r = progress(&l(&["==> 安装 Docker(源:A)", "Setting up docker-ce", "==> 源 A 安装失败,换下一个源重试", "==> 安装 Docker(源:B)"]));
        assert_eq!(r.1, "开始安装");
        // 等锁时列出的占用进程里有 docker-ce、apt-get install,不能当成本次安装的进度
        let w = progress(&l(&["==> 下载 Docker 官方安装脚本", "有其他包管理进程正在运行,等待它结束", "  占用: 99532 10:52 apt-get -y -qq install docker-ce docker-ce-cli"]));
        assert_eq!(w.1, "下载官方安装脚本");
    }

    #[test]
    fn speed_format() {
        assert_eq!(fmt_speed(0.14), "140 ms");
        assert_eq!(fmt_speed(1.234), "1.2 s");
    }

    #[test]
    fn ranked_puts_choice_first_then_fastest() {
        let p = Probe { speeds: vec![(Source::Official, None), (Source::Aliyun, Some(0.5)), (Source::Tencent, Some(0.2)), (Source::Netease, Some(0.9)), (Source::Cernet, Some(0.3))], ..Default::default() };
        assert_eq!(p.ranked(Source::Netease), vec![Source::Netease, Source::Tencent, Source::Cernet, Source::Aliyun]);
        assert_eq!(p.ranked(Source::Official), vec![Source::Official, Source::Tencent, Source::Cernet, Source::Aliyun]);
    }

    #[test]
    fn poll_parsing() {
        let p = parse_poll(b"ST=\nALIVE=1\nACCEL=1\nACCEL_DONE=0\n--LOG--\nhello\nwor");
        assert_eq!(p, Poll { status: None, alive: true, accel: true, accel_done: false, log: b"hello\nwor".to_vec() });
        let q = parse_poll(b"ST=0\nALIVE=0\nACCEL=0\nACCEL_DONE=0\n--LOG--\n");
        assert_eq!(q.status, Some(0));
        assert!(!q.alive && q.log.is_empty());
        assert_eq!(parse_poll(b"ST=100\nALIVE=0\n--LOG--\nx").status, Some(100));
        assert_eq!(parse_poll(b"").status, None);
    }

    #[test]
    fn explains_common_failures() {
        assert!(explain("E: Could not get lock /var/lib/dpkg/lock-frontend").unwrap().contains("占用"));
        assert!(explain("write error: No space left on device").unwrap().contains("磁盘"));
        assert!(explain("curl: (6) Could not resolve host: get.docker.com").unwrap().contains("DNS"));
        assert!(explain("curl: (28) Connection timed out after 5001 ms").unwrap().contains("网络"));
        assert!(explain("curl: (60) SSL certificate problem: certificate is not yet valid").unwrap().contains("时间"));
        assert!(explain("sudo: a password is required").unwrap().contains("root"));
        // 镜像同步到一半:apt 校验不过。日志里 curl -fsSL、ca-certificates 不能被当成证书问题
        let apt = "+ sh -c curl -fsSL https://x/gpg\nSetting up ca-certificates\nE: 无法下载 https://m/Release\n   Hashes of expected file:\nE: 部分索引文件下载失败。";
        assert!(explain(apt).unwrap().contains("镜像"));
        assert_eq!(explain("+ sh -c curl -fsSL https://x\nca-certificates is already the newest version"), None);
        // 同时有网络超时(前面下载脚本的重试)和哈希不符时,以哈希不符为准
        assert!(explain("curl: (28) Connection timed out\nHashes of expected file:").unwrap().contains("镜像"));
        assert_eq!(explain("something else"), None);
    }

    #[test]
    fn scripts_are_well_formed() {
        assert!(start_script(true).contains("touch /var/tmp/otter-docker-install/accel"));
        assert!(!start_script(false).contains("touch /var/tmp/otter-docker-install/accel\n"));
        assert_eq!(cleanup_script(false), "rm -rf /var/tmp/otter-docker-install");
        assert!(cleanup_script(true).contains("docker.list"));
        assert!(poll_script(10).contains("tail -c +11"));
        assert!(install_script(&[Source::Official]).contains("dpkg --configure -a"));
        let one = install_script(&[Source::Official]);
        assert!(one.contains("restore()") && one.contains("不再换源") && one.contains("rm -f \"$apt_list\""));
        let multi = install_script(&[Source::Tencent, Source::Official, Source::Aliyun]);
        assert!(multi.contains("\"腾讯云|https://mirrors.tencent.com/docker-ce\" \"官方|\" \"阿里云|"));
    }
}
