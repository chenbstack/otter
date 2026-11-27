//! 存储页挂载管理的真机用例:用回环设备做本地挂载(新挂、重复挂、编辑、取消持久化、换设备、失败回退、
//! 目录正忙时卸载被拒、卸载后 fstab 清干净、挂载点目录保留);NFS/CIFS 没装工具时的提示;
//! CIFS 凭据文件(用假的 `mount.cifs` 验证:权限 600、内容、fstab 里引用路径、密码不出现在 fstab 和命令行,卸载时一并删除)。
//! 真正的 NFS 服务端/Samba 服务端需要内核模块和装包,没做。

use super::harness::{Distro, Vm};
use crate::mounts::*;
use crate::sysops::{parse_storage, STORAGE_SCRIPT};

const RESET: &str = r#"
pkill -f 'sleep 600' 2>/dev/null
for m in /mnt/orbtest-*; do umount "$m" 2>/dev/null; done
# 回环设备是各虚拟机共用的内核资源,只能释放自己的镜像,不能 losetup -D
for f in /tmp/orbtest-mnt/a.img /tmp/orbtest-mnt/b.img; do [ -e $f ] && losetup -j $f 2>/dev/null | cut -d: -f1 | xargs -r losetup -d 2>/dev/null; done
rm -rf /mnt/orbtest-* /tmp/orbtest-mnt /root/.otter-cifs-mnt-orbtest-* /sbin/mount.cifs.orbtest
[ -e /etc/fstab ] && sed -i '/# otter$/d' /etc/fstab
rm -f /etc/fstab.otter.bak
[ -e /sbin/mount.cifs ] && grep -q orbtest-fake /sbin/mount.cifs 2>/dev/null && rm -f /sbin/mount.cifs
true
"#;

fn vm(d: Distro) -> super::Lease {
    Vm::shared(d, "mounts", RESET)
}

fn spec(src: &str, mount: &str, opts: &str, persist: bool) -> MountSpec {
    MountSpec { kind: MountKind::Local, source: src.into(), mount: mount.into(), fstype: "auto".into(), options: opts.into(), persist, user: String::new(), password: String::new() }
}

fn lines_for(vm: &Vm, file: &str, mount: &str) -> usize {
    vm.sh(&format!("awk -v m={mount} '$2==m' {file} | wc -l")).trim().parse().unwrap_or(99)
}

fn is_mounted(vm: &Vm, mount: &str) -> bool {
    lines_for(vm, "/proc/mounts", mount) > 0
}

fn local(d: Distro) {
    let vm = vm(d);
    if vm.sh("command -v mkfs.ext4 >/dev/null 2>&1 && echo ok").trim() != "ok" {
        return; // 模板机没有 mkfs.ext4(Rocky 默认如此),这条在别的发行版上测
    }
    let devs = vm.exec("准备两个回环设备", "mkdir -p /tmp/orbtest-mnt && cd /tmp/orbtest-mnt && for n in a b; do truncate -s 32M $n.img && mkfs.ext4 -q -F $n.img || exit 1; done; losetup -f --show a.img; losetup -f --show b.img");
    let devs: Vec<&str> = devs.lines().filter(|l| l.starts_with("/dev/loop")).collect();
    assert_eq!(devs.len(), 2, "{devs:?}");
    let (dev_a, dev_b) = (devs[0], devs[1]);
    let m = "/mnt/orbtest-a";
    let storage = || parse_storage(&vm.exec("读存储", STORAGE_SCRIPT)).unwrap();
    let existing = || storage().iter().map(|x| x.mount.clone()).collect::<Vec<_>>();

    // 1 新挂载:临时挂成功后写 fstab,第一次改之前备份
    let mut s = spec(dev_a, m, "defaults,nofail", true);
    validate(&s, false, &existing()).unwrap();
    let out = vm.exec("新挂载", &mount_script(&s, false));
    assert!(out.trim().ends_with("OK"), "{out}");
    assert!(is_mounted(&vm, m), "没挂上");
    assert_eq!(lines_for(&vm, "/etc/fstab", m), 1);
    assert!(vm.sh("tail -n 1 /etc/fstab").contains("# otter"));
    assert_eq!(vm.sh("[ -e /etc/fstab.otter.bak ] && echo yes").trim(), "yes");
    // 读回:存储页能认出它、可管理、带 fstab 那一行
    let list = storage();
    let e = list.iter().find(|x| x.mount == m).unwrap_or_else(|| panic!("存储读取里没有它:{list:?}"));
    assert!(e.managed && e.fstab.as_ref().is_some_and(|f| f.source == dev_a && f.options == "defaults,nofail"), "{e:?}");

    // 2 重复挂载同一位置:拒绝
    let (c, o) = vm.run(&mount_script(&s, false));
    assert!(c != 0 && o.contains("已经挂载"), "{c} {o}");

    // 3 编辑选项:挂载参数更新,fstab 仍只有一行
    vm.exec("写个文件", &format!("echo keep > {m}/f"));
    s.options = "defaults,nofail,noatime".into();
    vm.exec("编辑选项", &mount_script(&s, true));
    assert!(vm.sh(&format!("awk '$2==\"{m}\"' /proc/mounts")).contains("noatime"));
    assert_eq!(lines_for(&vm, "/etc/fstab", m), 1);
    assert!(vm.sh(&format!("awk '$2==\"{m}\"' /etc/fstab")).contains("noatime"));
    assert_eq!(vm.sh(&format!("cat {m}/f")).trim(), "keep", "重新挂载后文件还在");

    // 4 取消开机自动挂载:fstab 那行去掉,当前挂载保持
    s.persist = false;
    vm.exec("取消持久化", &mount_script(&s, true));
    assert!(is_mounted(&vm, m));
    assert_eq!(lines_for(&vm, "/etc/fstab", m), 0);

    // 5 换设备并重新持久化
    s.persist = true;
    s.source = dev_b.into();
    vm.exec("换设备", &mount_script(&s, true));
    assert!(vm.sh(&format!("awk '$2==\"{m}\" {{print $1}}' /proc/mounts")).contains(dev_b));
    assert_eq!(lines_for(&vm, "/etc/fstab", m), 1);

    // 6 编辑失败:原来的挂载恢复,fstab 不变
    let before = vm.sh("cat /etc/fstab");
    let mut bad = s.clone();
    bad.source = "/dev/orbtest-nonexistent".into();
    let (c, o) = vm.run(&mount_script(&bad, true));
    assert!(c != 0 && o.contains("已恢复原来的挂载"), "{c} {o}");
    assert!(vm.sh(&format!("awk '$2==\"{m}\" {{print $1}}' /proc/mounts")).contains(dev_b), "没恢复");
    assert_eq!(vm.sh("cat /etc/fstab"), before, "失败不该动 fstab");

    // 7 新挂载失败:fstab 不动,没留下半成品目录
    let bad = spec("/dev/orbtest-nonexistent", "/mnt/orbtest-bad", "defaults,nofail", true);
    let (c, o) = vm.run(&mount_script(&bad, false));
    assert!(c != 0, "{o}");
    assert_eq!(vm.sh("cat /etc/fstab"), before);
    assert_eq!(vm.sh("[ -e /mnt/orbtest-bad ] && echo exists").trim(), "", "新建的挂载点目录该回收");

    // 8 目录正忙:卸载被拒,什么都不改
    vm.exec("占用目录", &format!("cd {m} && (nohup sleep 600 >/dev/null 2>&1 &)"));
    let (c, o) = vm.run(&remove_script(m));
    assert!(c != 0 && o.contains("正在使用"), "{c} {o}");
    assert!(is_mounted(&vm, m));
    assert_eq!(vm.sh("cat /etc/fstab"), before);

    // 9 释放后卸载:fstab 清干净,挂载点目录和设备里的文件都还在
    vm.exec("释放", "pkill -f 'sleep 600'; sleep 1; true");
    let out = vm.exec("卸载", &remove_script(m));
    assert!(out.trim().ends_with("OK"), "{out}");
    assert!(!is_mounted(&vm, m));
    assert_eq!(lines_for(&vm, "/etc/fstab", m), 0);
    assert_eq!(vm.sh(&format!("[ -d {m} ] && echo dir")).trim(), "dir", "挂载点目录不该被删");
    // 其他行原样:没有 # otter 的行一行都没少
    assert_eq!(vm.sh("grep -vc '# otter$' /etc/fstab").trim(), vm.sh("grep -vc '# otter$' /etc/fstab.otter.bak").trim());
}
per_distro!(mounts_local, local);

fn tools_missing(d: Distro) {
    let vm = vm(d);
    for (kind, tool, hint) in [(MountKind::Nfs, "mount.nfs", "nfs"), (MountKind::Cifs, "mount.cifs", "cifs-utils")] {
        if vm.sh(&format!("command -v {tool} >/dev/null 2>&1 && echo have")).trim() == "have" {
            continue; // 模板机里装了就没法测这条
        }
        let mut s = spec("x", "/mnt/orbtest-t", "defaults", true);
        s.kind = kind;
        s.source = if kind == MountKind::Nfs { "10.0.0.1:/e".into() } else { "//10.0.0.1/s".into() };
        s.fstype = kind.default_fstype().into();
        let (c, o) = vm.run(&mount_script(&s, false));
        assert!(c != 0 && o.contains(hint), "{kind:?}: {c} {o}");
        assert_eq!(vm.sh("[ -e /mnt/orbtest-t ] && echo exists").trim(), "", "缺工具时不该建目录");
    }
}
per_distro!(mounts_tools_missing, tools_missing);

fn cifs_credentials(d: Distro) {
    let vm = vm(d);
    // 假的 mount.cifs:记下收到的参数和凭据文件内容,退出 0(不会真的出现在 /proc/mounts)
    vm.exec(
        "装假 mount.cifs",
        r#"mkdir -p /tmp/orbtest-mnt
cat > /sbin/mount.cifs <<'EOS'
#!/bin/sh
# orbtest-fake
echo "$@" > /tmp/orbtest-mnt/cifs-args
c=$(echo "$@" | sed -n 's/.*credentials=\([^, ]*\).*/\1/p')
[ -n "$c" ] && cp "$c" /tmp/orbtest-mnt/cifs-seen
exit 0
EOS
chmod +x /sbin/mount.cifs"#,
    );
    let m = "/mnt/orbtest-c";
    let mut s = spec("//10.0.0.9/share", m, "defaults,_netdev,nofail", true);
    s.kind = MountKind::Cifs;
    s.fstype = "cifs".into();
    s.user = "alice".into();
    s.password = "S3cret!pw".into();
    validate(&s, false, &["/".to_string()]).unwrap();
    // 密码走 stdin:脚本写进文件,再 `printf 密码 | sh 文件`
    let script = mount_script(&s, false);
    assert!(!script.contains("S3cret!pw"));
    vm.exec("写脚本", &format!("cat > /tmp/orbtest-mnt/add.sh <<'EOS'\n{script}\nEOS"));
    let out = vm.exec("挂载 CIFS", "printf '%s\\n' 'S3cret!pw' | sh /tmp/orbtest-mnt/add.sh");
    assert!(out.trim().ends_with("OK"), "{out}");
    let cred = "/root/.otter-cifs-mnt-orbtest-c";
    assert_eq!(vm.sh(&format!("stat -c '%a %U' {cred}")).trim(), "600 root");
    let content = vm.sh(&format!("cat {cred}"));
    assert!(content.contains("username=alice") && content.contains("password=S3cret!pw"), "{content}");
    // 挂载时看到的凭据文件有同样内容;密码不在命令行里,也不在 fstab 里
    assert!(vm.sh("cat /tmp/orbtest-mnt/cifs-seen").contains("password=S3cret!pw"));
    let args = vm.sh("cat /tmp/orbtest-mnt/cifs-args");
    assert!(!args.contains("S3cret"), "{args}");
    let fstab = vm.sh("cat /etc/fstab");
    assert!(fstab.contains(&format!("credentials={cred}")) && !fstab.contains("S3cret"), "{fstab}");
    assert_eq!(vm.sh(&format!("ls {cred}.new 2>/dev/null")).trim(), "", "临时凭据文件该转正");

    // 匿名:选项里是 guest,不建凭据文件
    let mut g = s.clone();
    g.mount = "/mnt/orbtest-g".into();
    g.user.clear();
    g.password.clear();
    vm.exec("匿名挂载", &mount_script(&g, false));
    assert!(vm.sh("cat /tmp/orbtest-mnt/cifs-args").contains("guest"));
    assert_eq!(vm.sh("ls /root/.otter-cifs-mnt-orbtest-g 2>/dev/null").trim(), "");

    // 卸载:fstab 的行和凭据文件一起清掉(假挂载没出现在 /proc/mounts,所以不用 umount)
    let out = vm.exec("卸载", &remove_script(m));
    assert!(out.trim().ends_with("OK"), "{out}");
    assert_eq!(vm.sh(&format!("ls {cred} 2>/dev/null")).trim(), "");
    assert_eq!(lines_for(&vm, "/etc/fstab", m), 0);
    vm.exec("清理", &remove_script("/mnt/orbtest-g"));
}
per_distro!(mounts_cifs_credentials, cifs_credentials);
