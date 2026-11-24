//! 服务器系统识别:节点连上之后,在已认证的 SSH 连接上读一次 `/etc/os-release` 和 `uname -s`,
//! 归类成 `OsKind` 存进节点,节点卡片据此换图标。服务器上不装任何东西(决策 0001)。

use std::sync::Arc;
use std::time::Duration;

use russh::client::Handle as SshHandle;

use crate::terminal::ssh::Client;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OsKind {
    /// 还没探测过,或认不出来
    #[default]
    Unknown,
    /// 认得出是 Linux 但不是下面列出的发行版
    Linux,
    Ubuntu,
    Debian,
    Centos,
    Rocky,
    Alma,
    /// RHEL 本身和认不出具体发行版的 RHEL 系(Oracle Linux、Amazon Linux 等)
    Rhel,
    Fedora,
    Alpine,
    Arch,
    OpenWrt,
    MacOs,
    FreeBsd,
}

impl OsKind {
    /// 存进库里的稳定字符串
    pub fn key(self) -> &'static str {
        match self {
            OsKind::Unknown => "",
            OsKind::Linux => "linux",
            OsKind::Ubuntu => "ubuntu",
            OsKind::Debian => "debian",
            OsKind::Centos => "centos",
            OsKind::Rocky => "rocky",
            OsKind::Alma => "alma",
            OsKind::Rhel => "rhel",
            OsKind::Fedora => "fedora",
            OsKind::Alpine => "alpine",
            OsKind::Arch => "arch",
            OsKind::OpenWrt => "openwrt",
            OsKind::MacOs => "macos",
            OsKind::FreeBsd => "freebsd",
        }
    }

    pub fn from_key(s: &str) -> OsKind {
        match s {
            "linux" => OsKind::Linux,
            "ubuntu" => OsKind::Ubuntu,
            "debian" => OsKind::Debian,
            "centos" => OsKind::Centos,
            "rocky" => OsKind::Rocky,
            "alma" => OsKind::Alma,
            "rhel" => OsKind::Rhel,
            "fedora" => OsKind::Fedora,
            "alpine" => OsKind::Alpine,
            "arch" => OsKind::Arch,
            "openwrt" => OsKind::OpenWrt,
            "macos" => OsKind::MacOs,
            "freebsd" => OsKind::FreeBsd,
            _ => OsKind::Unknown,
        }
    }

    /// 内置图标的资源路径(`assets/os/`,见 `src/assets.rs`);`None` 表示没有专用图标(macOS 用苹果图标,未知用服务器图标)
    pub fn logo(self) -> Option<&'static str> {
        match self {
            OsKind::Unknown | OsKind::MacOs => None,
            OsKind::Linux => Some("os/linux.svg"),
            OsKind::Ubuntu => Some("os/ubuntu.svg"),
            OsKind::Debian => Some("os/debian.svg"),
            OsKind::Centos => Some("os/centos.svg"),
            OsKind::Rocky => Some("os/rocky.svg"),
            OsKind::Alma => Some("os/alma.svg"),
            OsKind::Rhel => Some("os/redhat.svg"),
            OsKind::Fedora => Some("os/fedora.svg"),
            OsKind::Alpine => Some("os/alpine.svg"),
            OsKind::Arch => Some("os/arch.svg"),
            OsKind::OpenWrt => Some("os/openwrt.svg"),
            OsKind::FreeBsd => Some("os/freebsd.svg"),
        }
    }

    /// 品牌色(对应 tokens.md 的 `os.*`);`None` 表示用主题强调色
    pub fn hue(self) -> Option<u32> {
        match self {
            OsKind::Unknown | OsKind::Linux => None,
            OsKind::Ubuntu => Some(0xF47A3E),
            OsKind::Debian => Some(0xE0476B),
            OsKind::Centos => Some(0xB07CE0),
            OsKind::Rocky => Some(0x2FD39A),
            OsKind::Alma => Some(0x5AA9E6),
            OsKind::Rhel => Some(0xE5484D),
            OsKind::Fedora => Some(0x5B9BF5),
            OsKind::Alpine => Some(0x3FA9F5),
            OsKind::Arch => Some(0x4CC2FF),
            OsKind::OpenWrt => Some(0x4CC9A0),
            OsKind::MacOs => Some(0xC9CCE0),
            OsKind::FreeBsd => Some(0xD9534F),
        }
    }
}

/// 从 `/etc/os-release` 的内容和 `uname -s` 的输出判断系统。纯函数,方便测试。
pub fn parse(os_release: &str, uname: &str) -> OsKind {
    let field = |name: &str| -> String {
        os_release
            .lines()
            .find_map(|l| l.trim().strip_prefix(name).and_then(|r| r.strip_prefix('=')))
            .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_lowercase())
            .unwrap_or_default()
    };
    let id = field("ID");
    let like = field("ID_LIKE");
    let has = |w: &str| id == w || like.split_whitespace().any(|x| x == w);
    if !id.is_empty() {
        // 先认具体发行版,再按 ID_LIKE 归到上游(Mint → Ubuntu、Raspbian → Debian 等)
        if id == "openwrt" {
            return OsKind::OpenWrt;
        }
        if has("ubuntu") {
            return OsKind::Ubuntu;
        }
        if has("debian") {
            return OsKind::Debian;
        }
        match id.as_str() {
            "centos" => return OsKind::Centos,
            "rocky" => return OsKind::Rocky,
            "almalinux" => return OsKind::Alma,
            "fedora" => return OsKind::Fedora,
            _ => {}
        }
        if has("rhel") || has("centos") {
            return OsKind::Rhel;
        }
        if has("fedora") {
            return OsKind::Fedora;
        }
        if has("alpine") {
            return OsKind::Alpine;
        }
        if has("arch") {
            return OsKind::Arch;
        }
    }
    match uname.trim() {
        "Darwin" => OsKind::MacOs,
        "FreeBSD" => OsKind::FreeBsd,
        "Linux" => OsKind::Linux,
        _ if !id.is_empty() => OsKind::Linux,
        _ => OsKind::Unknown,
    }
}

const SEP: &str = "---OTTER-UNAME---";

/// 在已认证的连接上探测系统。读不到(命令不存在、超时)返回 `Unknown`,调用方不应据此覆盖已有结果。
pub async fn probe(session: &Arc<SshHandle<Client>>) -> OsKind {
    let cmd = format!("cat /etc/os-release 2>/dev/null; echo {SEP}; uname -s 2>/dev/null");
    let Ok(out) = crate::docker::exec(session, &cmd, None, Duration::from_secs(8)).await else {
        return OsKind::Unknown;
    };
    let (rel, uname) = out.stdout.split_once(SEP).unwrap_or((&out.stdout, ""));
    parse(rel, uname)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distros_from_os_release() {
        assert_eq!(parse("NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n", "Linux"), OsKind::Ubuntu);
        assert_eq!(parse("ID=debian\n", "Linux"), OsKind::Debian);
        assert_eq!(parse("ID=\"centos\"\nID_LIKE=\"rhel fedora\"\n", "Linux"), OsKind::Centos);
        assert_eq!(parse("ID=\"rhel\"\nID_LIKE=\"fedora\"\n", "Linux"), OsKind::Rhel);
        assert_eq!(parse("ID=almalinux\nID_LIKE=\"rhel centos fedora\"\n", "Linux"), OsKind::Alma);
        assert_eq!(parse("ID=ol\nID_LIKE=\"fedora\"\n", "Linux"), OsKind::Fedora);
        assert_eq!(parse("ID=rocky\nID_LIKE=\"rhel centos fedora\"\n", "Linux"), OsKind::Rocky);
        assert_eq!(parse("ID=fedora\n", "Linux"), OsKind::Fedora);
        assert_eq!(parse("ID=alpine\n", "Linux"), OsKind::Alpine);
        assert_eq!(parse("ID=arch\n", "Linux"), OsKind::Arch);
        assert_eq!(parse("ID=openwrt\nID_LIKE=\"lede openwrt\"\n", "Linux"), OsKind::OpenWrt);
    }

    #[test]
    fn derivatives_follow_upstream() {
        assert_eq!(parse("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n", "Linux"), OsKind::Ubuntu);
        assert_eq!(parse("ID=raspbian\nID_LIKE=debian\n", "Linux"), OsKind::Debian);
    }

    #[test]
    fn falls_back_to_uname() {
        assert_eq!(parse("", "Darwin\n"), OsKind::MacOs);
        assert_eq!(parse("", "FreeBSD\n"), OsKind::FreeBsd);
        assert_eq!(parse("", "Linux\n"), OsKind::Linux);
        assert_eq!(parse("ID=nixos\n", "Linux\n"), OsKind::Linux);
        assert_eq!(parse("", ""), OsKind::Unknown);
    }

    #[test]
    fn key_round_trip() {
        for k in [OsKind::Linux, OsKind::Ubuntu, OsKind::Debian, OsKind::Centos, OsKind::Rocky, OsKind::Alma, OsKind::Rhel, OsKind::Fedora, OsKind::Alpine, OsKind::Arch, OsKind::OpenWrt, OsKind::MacOs, OsKind::FreeBsd] {
            assert_eq!(OsKind::from_key(k.key()), k);
        }
        assert_eq!(OsKind::from_key(""), OsKind::Unknown);
    }
}

impl crate::Home {
    /// 终端连上、节点系统还没记录时,在它的连接上探测一次,结果写进节点和本地库。
    /// 每个节点每次运行只探测一次;探测失败(Unknown)不覆盖已有结果,下次启动再试。
    pub(crate) fn sync_os(&mut self, cx: &mut gpui::Context<Self>) {
        if self.demo {
            return;
        }
        let mut todo = Vec::new();
        for tab in &self.workspace.tabs {
            let (Some(id), Some(t)) = (tab.node(), tab.term.as_ref()) else { continue };
            if self.os_probed.contains(&id) || todo.iter().any(|(i, _)| *i == id) {
                continue;
            }
            let known = self.groups.iter().flat_map(|g| &g.nodes).find(|n| n.id == id).is_some_and(|n| n.os != OsKind::Unknown);
            if known {
                self.os_probed.insert(id);
            } else if let Some(sess) = t.handle.session() {
                todo.push((id, sess));
            }
        }
        for (id, sess) in todo {
            self.os_probed.insert(id);
            let (tx, rx) = tokio::sync::oneshot::channel();
            crate::terminal::ssh::runtime().spawn(async move {
                let _ = tx.send(probe(&sess).await);
            });
            cx.spawn(async move |this, cx| {
                if let Ok(kind) = rx.await {
                    let _ = this.update(cx, |this, cx| this.apply_os(id, kind, cx));
                }
            })
            .detach();
        }
    }

    fn apply_os(&mut self, id: crate::mock::NodeId, kind: OsKind, cx: &mut gpui::Context<Self>) {
        if kind == OsKind::Unknown {
            return;
        }
        if let Some(n) = self.groups.iter_mut().flat_map(|g| g.nodes.iter_mut()).find(|n| n.id == id) {
            n.os = kind;
        }
        if let Some(Err(e)) = self.store.as_mut().map(|s| s.set_node_os(id, kind.key())) {
            crate::logs::push(crate::logs::Level::Error, "本地库", &format!("保存节点系统失败: {e}"));
        }
        cx.notify();
    }
}
