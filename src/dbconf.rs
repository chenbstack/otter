//! 数据库配置文件的读取与保存脚本(纯函数,在服务器上以 root 经 `docker exec` 执行):
//! PostgreSQL 的 `postgresql.conf` / `pg_hba.conf`(数据目录里)、MySQL/MariaDB 的 `conf.d/otter.cnf`(Otter 自己的一份,不动镜像自带的)、Redis 启动参数里的 `*.conf`。
//! 保存前备份到 `/opt/otter/databases/conf-backup/<容器>/`;能校验的先校验(PostgreSQL 查 `pg_file_settings`/`pg_hba_file_rules`,MySQL 8 `mysqld --validate-config`),
//! 不通过就还原。不自动重启。设计见 docs/plans/active/2026-11-26-db-settings-tab.md。

use crate::dbinst::DbKind;
use crate::srvdata::{b64, DB};
use crate::sysops::quote;

pub struct ConfFile {
    pub name: &'static str,
    pub hint: &'static str,
}

pub fn files(kind: DbKind) -> &'static [ConfFile] {
    match kind {
        DbKind::Postgres => &[ConfFile { name: "postgresql.conf", hint: "主配置;保存后自动重载,需重启的参数要重启容器" }, ConfFile { name: "pg_hba.conf", hint: "客户端访问规则;保存后自动重载" }],
        DbKind::Mysql | DbKind::Mariadb => &[ConfFile { name: "otter.cnf", hint: "放在 conf.d 里,覆盖镜像默认值;保存后重启容器生效" }],
        DbKind::Redis => &[ConfFile { name: "redis.conf", hint: "启动参数里指定的配置文件;保存后重启容器生效" }],
    }
}

fn container_ok(c: &str) -> bool {
    !c.is_empty() && c.len() <= 128 && !c.starts_with(['.', '-', '_']) && c.chars().all(|x| x.is_ascii_alphanumeric() || matches!(x, '_' | '.' | '-'))
}

/// 在容器里解析出配置文件路径放进 `$F`(解析不到时 `$F` 为空)
fn resolve(kind: DbKind, file: usize) -> &'static str {
    match (kind, file) {
        (DbKind::Postgres, 1) => r#"F="${PGDATA:-/var/lib/postgresql/data}/pg_hba.conf""#,
        (DbKind::Postgres, _) => r#"F="${PGDATA:-/var/lib/postgresql/data}/postgresql.conf""#,
        (DbKind::Mysql | DbKind::Mariadb, _) => "F=/etc/mysql/conf.d/otter.cnf",
        (DbKind::Redis, _) => r#"F=$(tr '\0' '\n' </proc/1/cmdline | grep '\.conf$' | head -n1); if [ -z "$F" ]; then for x in /etc/redis/redis.conf /usr/local/etc/redis/redis.conf; do [ -f "$x" ] && F=$x && break; done; fi"#,
    }
}

/// 读:先输出一行 `@@M=<0|1>`(文件是否在挂载进来的目录里,0 表示在容器自己的文件系统,容器重建会丢),再输出内容;失败输出 `ERR:原因`
pub fn read_script(kind: DbKind, container: &str, file: usize) -> Result<String, String> {
    if !container_ok(container) {
        return Err("容器名不合法".into());
    }
    let inner = format!(
        r#"{}; [ -n "$F" ] || {{ echo 'ERR:没找到配置文件(这个 Redis 不是用配置文件启动的)'; exit 0; }}; m=0; while read -r _ mp _; do [ "$mp" != / ] && case "$F" in "$mp"/*) m=1;; esac; done </proc/mounts; echo "@@M=$m"; cat "$F" 2>/dev/null; true"#,
        resolve(kind, file)
    );
    let c = quote(container);
    Ok(format!("docker inspect -f '{{{{.State.Running}}}}' {c} 2>/dev/null | grep -q true || {{ echo 'ERR:容器没有在运行'; exit 0; }}\ndocker exec {c} sh -c {}\n", quote(&inner)))
}

/// 读回的输出 → (是否在挂载目录里, 内容)
pub fn parse_read(out: &str) -> Result<(bool, String), String> {
    if let Some(e) = out.trim_start().strip_prefix("ERR:") {
        return Err(e.trim().to_string());
    }
    let (first, rest) = out.split_once('\n').unwrap_or((out, ""));
    let m = first.trim().strip_prefix("@@M=").ok_or("读到的内容不对")?;
    Ok((m == "1", rest.to_string()))
}

/// 写:备份 → 写入 → 校验(能校验的)→ 重载(PostgreSQL),失败还原。输出 `OK` 或 `ERR:原因`
pub fn write_script(kind: DbKind, container: &str, file: usize, new: &str) -> Result<String, String> {
    if !container_ok(container) {
        return Err("容器名不合法".into());
    }
    let fname = files(kind).get(file).ok_or("文件序号不对")?.name;
    let c = quote(container);
    let check = match (kind, file) {
        (DbKind::Postgres, 1) => {
            let q = "SELECT line_number || ': ' || error FROM pg_hba_file_rules WHERE error IS NOT NULL;";
            format!("o=$(q {}) || {{ rb; echo \"ERR:$o\"; exit 0; }}\n[ -z \"$o\" ] || {{ rb; echo \"ERR:$o\"; exit 0; }}\n", quote(q))
        }
        (DbKind::Postgres, _) => {
            // 要重启才能生效的参数也会在 error 里写「could not be applied」,那不算错
            let q = "SELECT sourcefile || ':' || sourceline || ' ' || name || ': ' || error FROM pg_file_settings WHERE error IS NOT NULL AND error NOT LIKE '%could not be applied%';";
            format!("o=$(q {}) || {{ rb; echo \"ERR:$o\"; exit 0; }}\n[ -z \"$o\" ] || {{ rb; echo \"ERR:$o\"; exit 0; }}\n", quote(q))
        }
        (DbKind::Mysql, _) => format!("o=$(docker exec {c} mysqld --validate-config 2>&1) || {{ rb; echo \"ERR:$o\"; exit 0; }}\n"),
        _ => String::new(),
    };
    let reload = if kind == DbKind::Postgres { "o=$(q 'SELECT pg_reload_conf();') || { rb; echo \"ERR:$o\"; exit 0; }\n" } else { "" };
    let qfn = if kind == DbKind::Postgres { format!("q() {{ printf '%s\\n' \"$1\" | docker exec -i {c} sh -c {}; }}\n", quote(kind.client_script())) } else { String::new() };
    Ok(format!(
        r#"B={bk}/{container}; D={c}
docker inspect -f '{{{{.State.Running}}}}' $D 2>/dev/null | grep -q true || {{ echo 'ERR:容器没有在运行'; exit 0; }}
F=$(docker exec $D sh -c {resolve_print}); [ -n "$F" ] || {{ echo 'ERR:没找到配置文件'; exit 0; }}
mkdir -p "$B" || {{ echo 'ERR:建不了备份目录'; exit 0; }}
O=$(mktemp) || {{ echo 'ERR:建不了临时文件'; exit 0; }}
docker exec $D sh -c 'test -f "$1"' _ "$F" && E=1 || E=0
[ "$E" = 1 ] && docker exec $D cat "$F" > "$O" && cp "$O" "$B/{fname}.$(date +%s)"
rb() {{ if [ "$E" = 1 ]; then docker exec -i $D sh -c 'cat > "$1"' _ "$F" < "$O"; else docker exec $D rm -f "$F"; fi; rm -f "$O"; }}
{qfn}printf %s {body} | base64 -d | docker exec -i $D sh -c 'cat > "$1"' _ "$F" || {{ rb; echo 'ERR:写入失败'; exit 0; }}
{check}{reload}rm -f "$O"
echo OK
"#,
        bk = format!("{DB}/conf-backup"),
        resolve_print = quote(&format!("{}; printf %s \"$F\"", resolve(kind, file))),
        body = quote(&b64(new.as_bytes())),
    ))
}

/// pg_hba.conf 里 Otter 给某个用户写的来源限制,夹在这两行标记之间
fn hba_marks(user: &str) -> (String, String) {
    (format!("# otter:begin user={user}"), format!("# otter:end user={user}"))
}

/// 读出 Otter 给 `user` 写的允许来源(规范化后的 IP/网段),没有限制返回空
pub fn hba_ips(text: &str, user: &str) -> Vec<String> {
    let (b, e) = hba_marks(user);
    let mut on = false;
    let mut out = Vec::new();
    for l in text.lines().map(str::trim) {
        if l == b {
            on = true;
        } else if l == e {
            on = false;
        } else if on {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() >= 5 && f[0] == "host" && f[4] != "reject" && f[3] != "all" {
                out.push(f[3].to_string());
            }
        }
    }
    out
}

/// 认证方式:跟着文件里现有的 host 规则走(md5 / scram-sha-256),找不到用 scram-sha-256
fn hba_method(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 5 && f[0] == "host" && f[1] == "all" && f[2] == "all" && matches!(f[4], "md5" | "scram-sha-256" | "password")).then(|| f[4].to_string())
        })
        .next()
        .unwrap_or_else(|| "scram-sha-256".into())
}

/// 把 `user` 的来源限制改成 `ips`(空 = 去掉限制):旧的 Otter 块删掉,新块放在文件最前(pg_hba 先匹配先生效)。
/// 块内:每个允许的来源一条 host 规则,最后一条把这个用户的其它 TCP 来源拒绝。本地套接字(type local)不受影响。
pub fn hba_apply(text: &str, user: &str, ips: &[String]) -> String {
    let (b, e) = hba_marks(user);
    let mut rest: Vec<&str> = Vec::new();
    let mut skip = false;
    for l in text.lines() {
        let t = l.trim();
        if t == b {
            skip = true;
        } else if t == e {
            skip = false;
        } else if !skip {
            rest.push(l);
        }
    }
    let mut out = String::new();
    if !ips.is_empty() {
        let m = hba_method(text);
        out += &format!("{b}\n");
        for ip in ips {
            out += &format!("host all \"{user}\" {ip} {m}\n");
        }
        out += &format!("host all \"{user}\" all reject\n{e}\n");
    }
    out += &rest.join("\n");
    if !rest.is_empty() {
        out.push('\n');
    }
    out
}

/// 把已装实例(Otter 创建的,带 `otter.managed=db` 标签)的 `conf.d` 挂到主机:在 compose 里加 `./conf:/etc/mysql/conf.d`,
/// 先把容器里现有的 `otter.cnf` 拷到 `./conf`,再 `up -d` 重建容器(数据在 `./data` 不丢,会重启一次);起不来就还原 compose。输出 `OK` 或 `ERR:原因`
pub fn mount_script(container: &str) -> Result<String, String> {
    if !container_ok(container) {
        return Err("容器名不合法".into());
    }
    let c = quote(container);
    Ok(format!(
        r#"C={c}; B={DB}/conf-backup/$C
[ "$(docker inspect -f '{{{{index .Config.Labels "otter.managed"}}}}' $C 2>/dev/null)" = db ] || {{ echo 'ERR:只有 Otter 安装的实例才能改 compose;其它实例请自己在 compose 里加 ./conf:/etc/mysql/conf.d'; exit 0; }}
D=$(docker inspect -f '{{{{index .Config.Labels "com.docker.compose.project.working_dir"}}}}' $C)
F=$(docker inspect -f '{{{{index .Config.Labels "com.docker.compose.project.config_files"}}}}' $C | cut -d, -f1)
case "$D" in /opt/otter/*) ;; *) echo 'ERR:堆栈目录不在 /opt/otter 下,不改'; exit 0;; esac
[ -f "$F" ] || {{ echo 'ERR:找不到 compose 文件'; exit 0; }}
grep -q '/etc/mysql/conf.d' "$F" && {{ echo 'ERR:compose 里已经有 conf.d 的挂载'; exit 0; }}
N=$(grep -n '^      - \./data:/var/lib/mysql' "$F" | head -n1 | cut -d: -f1)
[ -n "$N" ] || {{ echo 'ERR:compose 的格式和 Otter 生成的不一样,请手动加 ./conf:/etc/mysql/conf.d'; exit 0; }}
if docker compose version >/dev/null 2>&1; then DC="docker compose"; else DC=docker-compose; fi
mkdir -p "$B" "$D/conf" || {{ echo 'ERR:建不了目录'; exit 0; }}
cp "$F" "$B/docker-compose.yml.$(date +%s)"
if docker exec $C test -f /etc/mysql/conf.d/otter.cnf 2>/dev/null; then docker exec $C cat /etc/mysql/conf.d/otter.cnf > "$D/conf/otter.cnf"; fi
awk -v n="$N" '{{print}} NR==n{{print "      - ./conf:/etc/mysql/conf.d"}}' "$F" > "$F.otter" && cat "$F.otter" > "$F" && rm -f "$F.otter" || {{ echo 'ERR:改 compose 失败'; exit 0; }}
o=$(cd "$D" && $DC -f "$F" up -d 2>&1) || {{ cp "$B"/docker-compose.yml.* "$F" 2>/dev/null; echo "ERR:$o"; exit 0; }}
i=0; while [ $i -lt 30 ]; do docker inspect -f '{{{{.State.Running}}}}' $C 2>/dev/null | grep -q true && break; i=$((i+1)); sleep 1; done
docker inspect -f '{{{{.State.Running}}}}' $C 2>/dev/null | grep -q true || {{ L=$(ls -t "$B"/docker-compose.yml.* | head -n1); cp "$L" "$F"; (cd "$D" && $DC -f "$F" up -d >/dev/null 2>&1); echo 'ERR:重建后容器没起来,已还原 compose'; exit 0; }}
echo OK
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hba_block() {
        let base = "# header\nlocal all all trust\nhost all all all scram-sha-256\n";
        let ips = vec!["10.0.0.0/8".to_string(), "1.2.3.4/32".to_string()];
        let t = hba_apply(base, "app", &ips);
        assert!(t.starts_with("# otter:begin user=app\nhost all \"app\" 10.0.0.0/8 scram-sha-256\n"));
        assert!(t.contains("host all \"app\" all reject\n# otter:end user=app\n# header"));
        assert_eq!(hba_ips(&t, "app"), ips);
        assert!(hba_ips(&t, "other").is_empty());
        // 再改一次:旧块被替换,其它用户的块不动
        let t2 = hba_apply(&t, "bob", &["5.5.5.5/32".to_string()]);
        let t3 = hba_apply(&t2, "app", &["9.9.9.9/32".to_string()]);
        assert_eq!(hba_ips(&t3, "app"), ["9.9.9.9/32"]);
        assert_eq!(hba_ips(&t3, "bob"), ["5.5.5.5/32"]);
        // 清空 = 回到原文
        assert_eq!(hba_apply(&hba_apply(base, "app", &ips), "app", &[]), base);
        assert_eq!(hba_method("host all all all md5\n"), "md5");
    }

    #[test]
    fn read_parse() {
        assert_eq!(parse_read("@@M=1\nport = 5432\n"), Ok((true, "port = 5432\n".into())));
        assert_eq!(parse_read("@@M=0\n"), Ok((false, String::new())));
        assert_eq!(parse_read("ERR:容器没有在运行\n"), Err("容器没有在运行".into()));
        assert!(parse_read("garbage").is_err());
    }

    #[test]
    fn mount_script_shape() {
        assert!(mount_script("a b").is_err());
        let s = mount_script("otter-mysql").unwrap();
        assert!(s.contains("otter.managed") && s.contains("./conf:/etc/mysql/conf.d") && s.contains("up -d") && s.contains("conf-backup/$C"));
    }

    #[test]
    fn scripts_are_safe() {
        assert!(read_script(DbKind::Postgres, "a;rm -rf /", 0).is_err());
        assert!(write_script(DbKind::Postgres, "$(x)", 0, "x").is_err());
        assert!(write_script(DbKind::Mysql, "my", 5, "x").is_err());
        // 内容走 base64,不会出现在脚本明文里
        let s = write_script(DbKind::Postgres, "otter-postgres", 0, "listen_addresses = '*'\n").unwrap();
        assert!(!s.contains("listen_addresses") && s.contains("pg_file_settings") && s.contains("pg_reload_conf") && s.contains("conf-backup/otter-postgres"));
        let h = write_script(DbKind::Postgres, "otter-postgres", 1, "host all all 0.0.0.0/0 scram-sha-256\n").unwrap();
        assert!(h.contains("pg_hba_file_rules") && h.contains("pg_hba.conf"));
        let m = write_script(DbKind::Mysql, "otter-mysql", 0, "[mysqld]\n").unwrap();
        assert!(m.contains("--validate-config") && !m.contains("pg_reload_conf"));
        let r = write_script(DbKind::Redis, "otter-redis", 0, "maxmemory 1gb\n").unwrap();
        assert!(!r.contains("--validate-config") && r.contains("cmdline"));
    }
}
