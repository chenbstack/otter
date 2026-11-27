//! 存储页的挂载管理:读 `/etc/fstab`、校验表单、生成挂载/编辑/卸载脚本。
//!
//! 只用服务器自带的 `mount` / `umount` 和 `/etc/fstab`,不装任何东西(决策 0001)。
//! 脚本都在 root 下执行;所有进入脚本的值先校验再 `quote`,CIFS 密码走 stdin,不出现在命令行或脚本文本里。

use crate::sysops::quote;

/// 存储页读取脚本里 fstab 部分的分隔标记
pub const FSTAB_MARK: &str = "@@FSTAB";

/// fstab 里的一行
#[derive(Clone, Debug, PartialEq)]
pub struct FstabEntry {
    pub source: String,
    pub mount: String,
    pub fstype: String,
    pub options: String,
}

fn unescape(s: &str) -> String {
    s.replace("\\040", " ").replace("\\011", "\t")
}

/// 解析 fstab:跳过空行和注释,至少要有来源、挂载点、类型三列
pub fn parse_fstab(text: &str) -> Vec<FstabEntry> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 3 {
                return None;
            }
            Some(FstabEntry { source: unescape(f[0]), mount: unescape(f[1]), fstype: f[2].to_string(), options: f.get(3).map(|s| s.to_string()).unwrap_or_default() })
        })
        .collect()
}

/// 系统挂载点:只读展示,不给编辑/删除
pub fn is_system(path: &str) -> bool {
    const EXACT: [&str; 12] = ["/", "/home", "/var", "/opt", "/srv", "/tmp", "/root", "/mnt", "/media", "/usr", "/boot", "/etc"];
    const PREFIX: [&str; 11] = ["/proc", "/sys", "/dev", "/run", "/boot", "/etc", "/usr", "/bin", "/sbin", "/lib", "/var/lib/docker"];
    EXACT.contains(&path) || PREFIX.iter().any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

/// 这个挂载点 Otter 能不能管:不是系统目录,并且写在 fstab 里,或者在 `/mnt`、`/media` 下
pub fn managed(mount: &str, fstab: &[FstabEntry]) -> bool {
    !is_system(mount) && (fstab.iter().any(|e| e.mount == mount) || mount.starts_with("/mnt/") || mount.starts_with("/media/"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MountKind {
    #[default]
    Local,
    Nfs,
    Cifs,
}

impl MountKind {
    pub fn label(self) -> &'static str {
        match self {
            MountKind::Local => "本地设备",
            MountKind::Nfs => "NFS",
            MountKind::Cifs => "CIFS(SMB)",
        }
    }

    pub fn default_options(self) -> &'static str {
        match self {
            MountKind::Local => "defaults,nofail",
            MountKind::Nfs | MountKind::Cifs => "defaults,_netdev,nofail",
        }
    }

    pub fn default_fstype(self) -> &'static str {
        match self {
            MountKind::Local => "auto",
            MountKind::Nfs => "nfs",
            MountKind::Cifs => "cifs",
        }
    }

    pub fn source_hint(self) -> &'static str {
        match self {
            MountKind::Local => "/dev/sdb1 或 UUID=xxxx",
            MountKind::Nfs => "192.168.1.10:/export/data",
            MountKind::Cifs => "//192.168.1.10/share",
        }
    }

    /// 按已有挂载的文件系统类型猜它属于哪一类(编辑时用)
    pub fn guess(fstype: &str) -> MountKind {
        match fstype {
            "nfs" | "nfs4" => MountKind::Nfs,
            "cifs" | "smb3" | "smbfs" => MountKind::Cifs,
            _ => MountKind::Local,
        }
    }
}

/// 表单填的内容
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MountSpec {
    pub kind: MountKind,
    pub source: String,
    pub mount: String,
    pub fstype: String,
    pub options: String,
    /// 写进 fstab(开机自动挂载)
    pub persist: bool,
    /// CIFS 用户名(空是匿名/guest)
    pub user: String,
    /// CIFS 密码;编辑时留空是沿用以前保存的
    pub password: String,
}

fn only(s: &str, extra: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// 校验表单。`editing` 时挂载点不能改,不再检查它;`existing` 是服务器上已有的挂载点
pub fn validate(spec: &MountSpec, editing: bool, existing: &[String]) -> Result<(), String> {
    let src = spec.source.trim();
    if src.is_empty() {
        return Err("来源不能为空".into());
    }
    match spec.kind {
        MountKind::Local => {
            let ok = (src.starts_with("/dev/") && only(src, "_./:-")) || ((src.starts_with("UUID=") || src.starts_with("LABEL=")) && only(&src[src.find('=').unwrap_or(0) + 1..], "_.-") && !src.ends_with('='));
            if !ok {
                return Err("本地设备写 /dev/sdb1,或 UUID=…、LABEL=…".into());
            }
        }
        MountKind::Nfs => {
            let ok = src.split_once(':').is_some_and(|(h, p)| !h.is_empty() && p.starts_with('/') && only(h, "_.-") && only(p, "_./-"));
            if !ok {
                return Err("NFS 来源写成 主机:/路径,例如 192.168.1.10:/export/data".into());
            }
        }
        MountKind::Cifs => {
            let ok = src.starts_with("//") && src[2..].split_once('/').is_some_and(|(h, s)| !h.is_empty() && !s.is_empty() && only(h, "_.-") && only(s, "_.$/-"));
            if !ok {
                return Err("CIFS 来源写成 //主机/共享名,例如 //192.168.1.10/share".into());
            }
        }
    }
    if !editing {
        validate_mount_point(spec.mount.trim(), existing)?;
    }
    let ft = spec.fstype.trim();
    if ft.is_empty() || !only(ft, "_-") {
        return Err("文件系统类型只能是字母、数字,例如 ext4、xfs、auto".into());
    }
    let o = spec.options.trim();
    if o.is_empty() || !only(o, "_=,.:/@-") || o.starts_with(',') || o.ends_with(',') || o.contains(",,") {
        return Err("挂载选项用逗号分隔,例如 defaults,nofail".into());
    }
    if spec.kind == MountKind::Cifs {
        if o.split(',').any(|x| ["credentials", "username", "user", "password", "pass", "guest"].contains(&x.split('=').next().unwrap_or(""))) {
            return Err("用户名和密码在下面单独填,选项里不要写 credentials、username、password、guest".into());
        }
        if spec.user.contains(['\n', '\r', '\0']) || spec.password.contains(['\n', '\r', '\0']) {
            return Err("用户名和密码不能换行".into());
        }
    }
    Ok(())
}

pub fn validate_mount_point(m: &str, existing: &[String]) -> Result<(), String> {
    if !m.starts_with('/') || m.len() < 2 || m.ends_with('/') || !only(m, "_./-") || m.contains("//") || m.split('/').any(|p| p == ".." || p == ".") {
        return Err("挂载点写绝对路径,例如 /mnt/data(字母、数字和 _ . - /)".into());
    }
    if is_system(m) && !m.starts_with("/mnt/") && !m.starts_with("/media/") {
        return Err(format!("{m} 是系统目录,请换一个,例如 /mnt/data"));
    }
    for e in existing.iter().filter(|e| e.as_str() != "/") {
        if e == m {
            return Err(format!("{m} 已经挂载了别的存储,请先卸载"));
        }
        if m.starts_with(&format!("{e}/")) {
            return Err(format!("{m} 在已有挂载点 {e} 里面,请换一个位置"));
        }
        if e.starts_with(&format!("{m}/")) {
            return Err(format!("{m} 会盖住里面已有的挂载点 {e},请换一个位置"));
        }
    }
    Ok(())
}

/// CIFS 凭据文件路径:`/root/.otter-cifs-<挂载点>`,按挂载点命名,删除挂载时一并删
pub fn cred_path(mount: &str) -> String {
    let name: String = mount.trim_matches('/').chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' { c } else { '-' }).collect();
    format!("/root/.otter-cifs-{name}")
}

/// 一行 awk:去掉 fstab 里挂载点是 `$M` 的行(注释和别的行原样不动),结果写到 `$tmp`
const FSTAB_DROP: &str = r#"awk -v m="$M" '{ if ($0 !~ /^[ \t]*#/ && NF >= 2) { x = $2; gsub(/\\040/, " ", x); if (x == m) next } print }' /etc/fstab > "$tmp""#;

const PRELUDE: &str = r#"set -u
fail() { echo "$1" >&2; exit 1; }
tail1() { printf '%s' "$1" | tail -n 1; }
TO=; command -v timeout >/dev/null 2>&1 && TO='timeout 30'
mounted() { awk -v m="$M" '{ x = $2; gsub(/\\040/, " ", x); if (x == m) f = 1 } END { exit !f }' /proc/mounts 2>/dev/null; }
"#;

/// 改 fstab 之前备份一次,改完让 systemd 重新生成挂载单元
const FSTAB_BACKUP: &str = "[ -e /etc/fstab ] || : > /etc/fstab\n[ -e /etc/fstab.otter.bak ] || cp -p /etc/fstab /etc/fstab.otter.bak 2>/dev/null\ntmp=$(mktemp) || fail '不能创建临时文件'\n";
const FSTAB_RELOAD: &str = "command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload >/dev/null 2>&1\n";

/// 新挂载或编辑已有挂载。先临时挂载,成功后才改 fstab;失败原样回退。
/// CIFS 且填了密码时,密码是 stdin 的第一行。
pub fn mount_script(spec: &MountSpec, editing: bool) -> String {
    let cifs = spec.kind == MountKind::Cifs;
    let mut s = String::from(PRELUDE);
    s.push_str(&format!("M={}\nSRC={}\nT={}\nO={}\n", quote(spec.mount.trim()), quote(spec.source.trim()), quote(spec.fstype.trim()), quote(spec.options.trim())));
    s.push_str("TRY=\"$O\"; FO=\"$O\"; CRED=; PWFILE=; CREATED=0\n");
    match spec.kind {
        MountKind::Nfs => s.push_str("command -v mount.nfs >/dev/null 2>&1 || fail '服务器上没有 mount.nfs:请先安装 nfs-common(Debian、Ubuntu)或 nfs-utils(RHEL 系)'\n"),
        MountKind::Cifs => s.push_str("command -v mount.cifs >/dev/null 2>&1 || fail '服务器上没有 mount.cifs:请先安装 cifs-utils'\n"),
        MountKind::Local => {}
    }
    if cifs {
        if spec.user.trim().is_empty() {
            s.push_str("TRY=\"$O,guest\"; FO=\"$O,guest\"\n");
        } else {
            s.push_str(&format!("CRED={}\nPWFILE=\"$CRED.new\"\n", quote(&cred_path(spec.mount.trim()))));
            // 密码:这次填了就用 stdin 的,没填(编辑时)沿用以前保存的
            if spec.password.is_empty() {
                s.push_str("PW=$(sed -n 's/^password=//p' \"$CRED\" 2>/dev/null | head -n 1)\n");
            } else {
                s.push_str("IFS= read -r PW\n");
            }
            s.push_str(&format!(
                "( umask 077; printf 'username=%s\\npassword=%s\\n' {} \"$PW\" > \"$PWFILE\" ) || fail '不能保存凭据文件'\nTRY=\"$O,credentials=$PWFILE\"; FO=\"$O,credentials=$CRED\"\n",
                quote(spec.user.trim())
            ));
        }
    }
    // 试挂载时去掉 nofail:带着它 mount 在设备不存在时也返回成功,会掩盖失败(写 fstab 的选项不受影响)
    s.push_str("TRY=$(printf '%s' \"$TRY\" | tr ',' '\\n' | grep -vx nofail | paste -sd, -); [ -n \"$TRY\" ] || TRY=defaults\n");
    // 取消临时凭据文件
    s.push_str("cleanup() { [ \"$CREATED\" = 1 ] && rmdir \"$M\" 2>/dev/null; [ -n \"$PWFILE\" ] && rm -f \"$PWFILE\"; }\n");
    s.push_str("[ -d \"$M\" ] || { mkdir -p \"$M\" 2>/dev/null && CREATED=1 || { cleanup; fail \"不能创建挂载点目录 $M\"; }; }\n");
    s.push_str("WAS=0\nif mounted; then\n");
    if editing {
        s.push_str(concat!(
            "  WAS=1\n",
            "  OSRC=$(awk -v m=\"$M\" '{ x = $2; gsub(/\\\\040/, \" \", x); if (x == m) { print $1; exit } }' /proc/mounts)\n",
            "  OT=$(awk -v m=\"$M\" '{ x = $2; gsub(/\\\\040/, \" \", x); if (x == m) { print $3; exit } }' /proc/mounts)\n",
            "  OOPT=$(awk -v m=\"$M\" '{ x = $2; gsub(/\\\\040/, \" \", x); if (x == m) { print $4; exit } }' /proc/mounts)\n",
            "  e=$($TO umount \"$M\" 2>&1) || { cleanup; fail \"卸载旧挂载失败(目录可能正在使用):$(tail1 \"$e\")\"; }\n",
        ));
    } else {
        s.push_str("  cleanup; fail \"$M 已经挂载了别的存储,请先卸载\"\n");
    }
    s.push_str("fi\n");
    s.push_str(concat!(
        "if [ \"$T\" = auto ]; then e=$($TO mount -o \"$TRY\" \"$SRC\" \"$M\" 2>&1); else e=$($TO mount -t \"$T\" -o \"$TRY\" \"$SRC\" \"$M\" 2>&1); fi\n",
        "rc=$?\n",
        "if [ $rc -ne 0 ]; then\n",
        "  cleanup\n",
        "  back=\n",
        "  if [ \"$WAS\" = 1 ]; then\n",
        "    if awk -v m=\"$M\" '{ x = $2; gsub(/\\\\040/, \" \", x); if ($0 !~ /^[ \\t]*#/ && x == m) f = 1 } END { exit !f }' /etc/fstab 2>/dev/null; then\n",
        "      $TO mount \"$M\" >/dev/null 2>&1 && back=1\n",
        "    else\n",
        "      $TO mount -t \"$OT\" -o \"$OOPT\" \"$OSRC\" \"$M\" >/dev/null 2>&1 && back=1\n",
        "    fi\n",
        "    [ -n \"$back\" ] && back=';已恢复原来的挂载' || back=';原来的挂载没能恢复,请检查'\n",
        "  fi\n",
        "  fail \"$(tail1 \"$e\")$back\"\n",
        "fi\n",
    ));
    // 成功:凭据文件转正
    if cifs && !spec.user.trim().is_empty() {
        s.push_str("mv -f \"$PWFILE\" \"$CRED\" && chmod 600 \"$CRED\"\n");
    }
    // fstab
    if spec.persist {
        s.push_str(FSTAB_BACKUP);
        s.push_str(&format!("{FSTAB_DROP} || {{ rm -f \"$tmp\"; fail '读取 /etc/fstab 失败'; }}\n"));
        s.push_str("FM=$(printf '%s' \"$M\" | sed 's/ /\\\\040/g')\n");
        s.push_str("printf '%s %s %s %s 0 0 # otter\\n' \"$SRC\" \"$FM\" \"$T\" \"$FO\" >> \"$tmp\" && cat \"$tmp\" > /etc/fstab || { rm -f \"$tmp\"; fail '已挂载,但写入 /etc/fstab 失败'; }\nrm -f \"$tmp\"\n");
        s.push_str(FSTAB_RELOAD);
    } else if editing {
        // 编辑时取消了「开机自动挂载」:把 fstab 里的那一行去掉,只留当前挂载
        s.push_str(FSTAB_BACKUP);
        s.push_str(&format!("{FSTAB_DROP} && cat \"$tmp\" > /etc/fstab\nrm -f \"$tmp\"\n"));
        s.push_str(FSTAB_RELOAD);
    }
    s.push_str("echo OK\n");
    s
}

/// 卸载并从 fstab 移除。不删文件、不删挂载点目录;目录正忙就报错,不强制、不 lazy。
pub fn remove_script(mount: &str) -> String {
    let mut s = String::from(PRELUDE);
    s.push_str(&format!("M={}\n", quote(mount)));
    s.push_str("if mounted; then e=$($TO umount \"$M\" 2>&1) || fail \"目录可能正在使用:$(tail1 \"$e\")\"; fi\n");
    s.push_str(r#"CRED=$(awk -v m="$M" '{ if ($0 !~ /^[ \t]*#/ && NF >= 4) { x = $2; gsub(/\\040/, " ", x); if (x == m) print $4 } }' /etc/fstab 2>/dev/null | tr ',' '\n' | sed -n 's|^credentials=\(/root/\.otter-cifs-[A-Za-z0-9._-]*\)$|\1|p' | head -n 1)
"#);
    s.push_str(FSTAB_BACKUP);
    s.push_str(&format!("{FSTAB_DROP} && cat \"$tmp\" > /etc/fstab\nrm -f \"$tmp\"\n"));
    s.push_str("[ -n \"$CRED\" ] && rm -f -- \"$CRED\" \"$CRED.new\"\n");
    s.push_str(FSTAB_RELOAD);
    s.push_str("echo OK\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: MountKind) -> MountSpec {
        MountSpec {
            kind,
            source: match kind {
                MountKind::Local => "/dev/sdb1".into(),
                MountKind::Nfs => "10.0.0.5:/export/data".into(),
                MountKind::Cifs => "//10.0.0.5/share".into(),
            },
            mount: "/mnt/data".into(),
            fstype: kind.default_fstype().into(),
            options: kind.default_options().into(),
            persist: true,
            user: String::new(),
            password: String::new(),
        }
    }

    fn sh_ok(script: &str) {
        let out = std::process::Command::new("sh").arg("-n").arg("-c").arg(script).output().unwrap();
        assert!(out.status.success(), "语法错误:{}\n{script}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn parse_fstab_skips_comments_and_unescapes() {
        let t = "# comment\n\nUUID=abc / ext4 defaults 0 1\n10.0.0.5:/e /mnt/my\\040disk nfs defaults,_netdev 0 0 # otter\nshort\n";
        let v = parse_fstab(t);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].mount, "/mnt/my disk");
        assert_eq!(v[1].fstype, "nfs");
        assert_eq!(v[1].options, "defaults,_netdev");
    }

    #[test]
    fn managed_rules() {
        let fs = parse_fstab("/dev/sdb1 /data ext4 defaults 0 2\n/dev/sda1 /home ext4 defaults 0 2\n");
        assert!(managed("/data", &fs));
        assert!(managed("/mnt/x", &fs));
        assert!(managed("/media/usb", &fs));
        assert!(!managed("/home", &fs));
        assert!(!managed("/", &fs));
        assert!(!managed("/boot/efi", &fs));
        assert!(!managed("/mnt", &fs));
        assert!(!managed("/other", &fs));
    }

    #[test]
    fn validate_sources() {
        let ex: Vec<String> = vec!["/".into()];
        for k in [MountKind::Local, MountKind::Nfs, MountKind::Cifs] {
            assert!(validate(&spec(k), false, &ex).is_ok(), "{k:?}");
        }
        let mut s = spec(MountKind::Local);
        for bad in ["sdb1", "/dev/sdb1; rm -rf /", "UUID=", "/dev/s db", "/dev/$(x)"] {
            s.source = bad.into();
            assert!(validate(&s, false, &ex).is_err(), "{bad}");
        }
        s.source = "UUID=1234-ABCD".into();
        assert!(validate(&s, false, &ex).is_ok());
        let mut n = spec(MountKind::Nfs);
        for bad in ["host/path", ":/x", "h:x", "h:/a b", "h:/a;b"] {
            n.source = bad.into();
            assert!(validate(&n, false, &ex).is_err(), "{bad}");
        }
        let mut c = spec(MountKind::Cifs);
        for bad in ["/h/s", "//h", "//h/", "//h/a b", "//h/a'b"] {
            c.source = bad.into();
            assert!(validate(&c, false, &ex).is_err(), "{bad}");
        }
    }

    #[test]
    fn validate_mount_point_rules() {
        let ex: Vec<String> = vec!["/".into(), "/data".into(), "/mnt/nas".into()];
        assert!(validate_mount_point("/mnt/x", &ex).is_ok());
        for bad in ["mnt/x", "/", "/mnt/x/", "/mnt/a b", "/mnt/../etc", "/etc/x", "/usr/local/x", "/boot", "/proc/x", "/mnt/a'b", "/mnt//x", "/home", "/data", "/data/sub", "/mnt/nas/sub", "/mnt/$x"] {
            assert!(validate_mount_point(bad, &ex).is_err(), "{bad}");
        }
        // 会盖住里面已有的挂载点
        let ex2: Vec<String> = vec!["/".into(), "/srv2/a".into()];
        assert!(validate_mount_point("/srv2", &ex2).is_err());
        // /mnt、/media 下面可以
        assert!(validate_mount_point("/media/usb", &ex).is_ok());
    }

    #[test]
    fn validate_options_and_cifs() {
        let ex: Vec<String> = vec!["/".into()];
        let mut s = spec(MountKind::Local);
        for bad in ["", "defaults;rm", ",defaults", "defaults,", "a,,b", "a b", "a$b"] {
            s.options = bad.into();
            assert!(validate(&s, false, &ex).is_err(), "{bad:?}");
        }
        let mut c = spec(MountKind::Cifs);
        for bad in ["defaults,password=x", "credentials=/etc/x", "username=a", "guest"] {
            c.options = bad.into();
            assert!(validate(&c, false, &ex).is_err(), "{bad}");
        }
        c.options = "defaults,vers=3.0,uid=1000".into();
        assert!(validate(&c, false, &ex).is_ok());
        c.password = "a\nb".into();
        assert!(validate(&c, false, &ex).is_err());
        // 编辑时挂载点不再检查
        let mut e = spec(MountKind::Local);
        e.mount = "/boot".into();
        assert!(validate(&e, true, &ex).is_ok());
    }

    #[test]
    fn cred_path_is_safe() {
        assert_eq!(cred_path("/mnt/nas"), "/root/.otter-cifs-mnt-nas");
        assert_eq!(cred_path("/mnt/my disk"), "/root/.otter-cifs-mnt-my-disk");
    }

    #[test]
    fn scripts_have_valid_syntax() {
        for k in [MountKind::Local, MountKind::Nfs, MountKind::Cifs] {
            for persist in [true, false] {
                for editing in [true, false] {
                    let mut s = spec(k);
                    s.persist = persist;
                    sh_ok(&mount_script(&s, editing));
                }
            }
        }
        let mut c = spec(MountKind::Cifs);
        c.user = "alice".into();
        sh_ok(&mount_script(&c, false));
        c.password = "p'w\"$x".into();
        sh_ok(&mount_script(&c, true));
        sh_ok(&remove_script("/mnt/data"));
        sh_ok(&remove_script("/mnt/it's"));
    }

    #[test]
    fn password_never_in_script() {
        let mut c = spec(MountKind::Cifs);
        c.user = "alice".into();
        c.password = "S3cretPw".into();
        let s = mount_script(&c, false);
        assert!(!s.contains("S3cretPw"));
        assert!(s.contains("IFS= read -r PW"));
        assert!(s.contains("'alice'"));
        assert!(s.contains("credentials=$CRED"));
        // 编辑时没填密码:沿用旧凭据,不读 stdin
        c.password.clear();
        let s = mount_script(&c, true);
        assert!(!s.contains("read -r PW"));
        assert!(s.contains("password=//p"));
    }

    #[test]
    fn script_quotes_values() {
        let mut s = spec(MountKind::Local);
        s.mount = "/mnt/o'k".into();
        let out = mount_script(&s, false);
        assert!(out.contains(r"M='/mnt/o'\''k'"));
        assert!(mount_script(&spec(MountKind::Cifs), false).contains("TRY=\"$O,guest\""));
        assert!(mount_script(&spec(MountKind::Nfs), false).contains("mount.nfs"));
        // 不持久化的新挂载不碰 fstab
        let mut s = spec(MountKind::Local);
        s.persist = false;
        assert!(!mount_script(&s, false).contains("/etc/fstab\n"));
        // 取消持久化的编辑要把 fstab 那行去掉
        assert!(mount_script(&s, true).contains("cat \"$tmp\" > /etc/fstab"));
    }
}
