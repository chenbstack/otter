//! 数据库「设置」页的常用参数:参数表、读取、校验、生成写入语句(纯函数)。
//! 都在容器里经客户端执行(见 `dbinst`),不改配置文件:PostgreSQL 用 `ALTER SYSTEM`(写数据目录里的 `postgresql.auto.conf`)、
//! MySQL 8 用 `SET PERSIST`(`mysqld-auto.cnf`)、Redis 用 `CONFIG SET` + `CONFIG REWRITE`。设计见 docs/plans/active/2026-11-26-db-settings-tab.md。

use crate::dbinst::DbKind;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PKind {
    /// 整数(含范围)
    Int(i64, i64),
    /// 小数(秒)
    Dec,
    /// 容量:`128MB`、`1GB`;MySQL/Redis 内部是字节,界面用 `128M`
    Size,
    OnOff,
    Enum(&'static [&'static str]),
    /// 时区 `+08:00` / `SYSTEM`
    Zone,
}

pub struct Param {
    pub name: &'static str,
    pub label: &'static str,
    pub desc: &'static str,
    /// 默认值(界面显示,也是 Redis 「恢复默认」写回的值)
    pub default: &'static str,
    pub kind: PKind,
    /// 改了要重启才生效
    pub restart: bool,
}

const fn p(name: &'static str, label: &'static str, desc: &'static str, default: &'static str, kind: PKind, restart: bool) -> Param {
    Param { name, label, desc, default, kind, restart }
}

const PG: &[Param] = &[
    p("max_connections", "最大连接数", "同时允许的客户端连接数;调大会多占内存", "100", PKind::Int(1, 10000), true),
    p("shared_buffers", "共享缓冲区", "缓存数据页的内存,常设为物理内存的 25% 左右", "128MB", PKind::Size, true),
    p("work_mem", "排序内存", "每个排序、哈希操作可用的内存;并发高时别设太大", "4MB", PKind::Size, false),
    p("maintenance_work_mem", "维护内存", "建索引、VACUUM 等维护操作可用的内存", "64MB", PKind::Size, false),
    p("effective_cache_size", "可用缓存估计", "优化器估计的磁盘缓存总量(含系统缓存),只影响查询计划,不占内存", "4GB", PKind::Size, false),
    p("max_wal_size", "WAL 上限", "两次检查点之间 WAL 最大体积,调大可减少写入抖动", "1GB", PKind::Size, false),
    p("log_min_duration_statement", "慢查询阈值(毫秒)", "执行超过这个时间的语句记到日志;-1 关闭,0 记录全部", "-1", PKind::Int(-1, 2_147_483_647), false),
    p("statement_timeout", "语句超时(毫秒)", "单条语句最长执行时间,0 不限制", "0", PKind::Int(0, 2_147_483_647), false),
    p("idle_in_transaction_session_timeout", "空闲事务超时(毫秒)", "事务开着却不动超过这个时间就断开连接,0 不限制", "0", PKind::Int(0, 2_147_483_647), false),
];

const MYSQL: &[Param] = &[
    p("max_connections", "最大连接数", "同时允许的客户端连接数", "151", PKind::Int(1, 100_000), false),
    p("innodb_buffer_pool_size", "缓冲池大小", "InnoDB 缓存数据和索引的内存,专用数据库常设为物理内存的 50%~70%", "128M", PKind::Size, false),
    p("max_allowed_packet", "最大包大小", "单条语句或单行数据的最大体积;存大字段、导入大文件时调大", "64M", PKind::Size, false),
    p("wait_timeout", "空闲连接超时(秒)", "空闲连接多久后被服务器断开", "28800", PKind::Int(1, 31_536_000), false),
    p("slow_query_log", "慢查询日志", "记录执行时间超过阈值的语句", "OFF", PKind::OnOff, false),
    p("long_query_time", "慢查询阈值(秒)", "超过这个时间算慢查询,可以带小数,如 0.5", "10", PKind::Dec, false),
    p("character_set_server", "默认字符集", "新建库默认用的字符集;已有的库不受影响", "utf8mb4", PKind::Enum(&["utf8mb4", "utf8mb3", "latin1"]), false),
    p("time_zone", "时区", "如 +08:00;SYSTEM 表示跟随系统", "SYSTEM", PKind::Zone, false),
];

const REDIS: &[Param] = &[
    p("maxmemory", "内存上限", "用到这个量就按淘汰策略清理;0 表示不限制", "0", PKind::Size, false),
    p("maxmemory-policy", "淘汰策略", "内存满了先清理哪些 key;noeviction 不清理,写入会报错", "noeviction", PKind::Enum(&["noeviction", "allkeys-lru", "allkeys-lfu", "volatile-lru", "volatile-lfu", "volatile-ttl", "allkeys-random", "volatile-random"]), false),
    p("appendonly", "AOF 持久化", "每条写命令追加到日志,丢数据更少,占磁盘更多", "no", PKind::OnOff, false),
    p("timeout", "空闲连接超时(秒)", "客户端空闲多久后断开,0 不断开", "0", PKind::Int(0, 31_536_000), false),
    p("tcp-keepalive", "TCP 保活(秒)", "向客户端发保活探测的间隔,0 关闭", "300", PKind::Int(0, 86_400), false),
    p("slowlog-log-slower-than", "慢日志阈值(微秒)", "执行超过这个时间的命令进慢日志;-1 关闭", "10000", PKind::Int(-1, 2_147_483_647), false),
];

/// 这个类型有没有参数表(MariaDB 没有 `SET PERSIST`,暂不支持)
pub fn supported(kind: DbKind) -> bool {
    !matches!(kind, DbKind::Mariadb)
}

pub fn params(kind: DbKind) -> &'static [Param] {
    match kind {
        DbKind::Postgres => PG,
        DbKind::Mysql => MYSQL,
        DbKind::Redis => REDIS,
        DbKind::Mariadb => &[],
    }
}

/// 读到的一项:显示用的当前值、是否要重启后才生效(PostgreSQL)
#[derive(Clone, Debug, PartialEq)]
pub struct Cur {
    pub value: String,
    pub pending: bool,
}

/// 读当前值的语句(stdin 传给客户端)
pub fn read_sql(kind: DbKind) -> String {
    let ps = params(kind);
    match kind {
        DbKind::Postgres => {
            let names = ps.iter().map(|x| format!("'{}'", x.name)).collect::<Vec<_>>().join(",");
            format!("SELECT n, current_setting(n), COALESCE((SELECT pending_restart FROM pg_settings WHERE name = n), false) FROM unnest(ARRAY[{names}]) AS n;\n")
        }
        DbKind::Mysql => {
            let names = ps.iter().map(|x| format!("'{}'", x.name)).collect::<Vec<_>>().join(",");
            format!("SHOW GLOBAL VARIABLES WHERE Variable_name IN ({names});\n")
        }
        DbKind::Redis => ps.iter().map(|x| format!("CONFIG GET {}\n", x.name)).collect(),
        DbKind::Mariadb => String::new(),
    }
}

/// 字节数 → `128M` 这样的写法(整除才用大单位)
pub fn human_bytes(n: u64) -> String {
    const U: [(&str, u64); 4] = [("T", 1 << 40), ("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)];
    if n == 0 {
        return "0".into();
    }
    for (u, v) in U {
        if n % v == 0 {
            return format!("{}{u}", n / v);
        }
    }
    n.to_string()
}

/// 把读到的原始值整理成界面显示的写法
fn display(kind: DbKind, p: &Param, raw: &str) -> String {
    let raw = raw.trim();
    match (kind, p.kind) {
        (DbKind::Mysql | DbKind::Redis, PKind::Size) => raw.parse::<u64>().map(human_bytes).unwrap_or_else(|_| raw.to_string()),
        (DbKind::Redis, PKind::OnOff) => raw.to_ascii_lowercase(),
        (DbKind::Mysql, PKind::OnOff) => if matches!(raw, "1" | "ON" | "on") { "ON".into() } else { "OFF".into() },
        _ => raw.to_string(),
    }
}

/// 读回的输出 → 与 `params(kind)` 对齐的当前值
pub fn parse_current(kind: DbKind, out: &str) -> Vec<Option<Cur>> {
    let ps = params(kind);
    let mut map: std::collections::HashMap<String, (String, bool)> = Default::default();
    match kind {
        DbKind::Postgres => {
            for l in out.lines() {
                let f: Vec<&str> = l.split('\t').collect();
                if f.len() >= 2 {
                    map.insert(f[0].to_string(), (f[1].to_string(), f.get(2).is_some_and(|x| x.trim() == "t")));
                }
            }
        }
        DbKind::Mysql => {
            for l in out.lines() {
                if let Some((k, v)) = l.split_once('\t') {
                    map.insert(k.to_string(), (v.to_string(), false));
                }
            }
        }
        DbKind::Redis => {
            // CONFIG GET 每条输出两行:名称、值(值可能是空行)
            let lines: Vec<&str> = out.lines().collect();
            let mut i = 0;
            while i < lines.len() {
                if ps.iter().any(|x| x.name == lines[i].trim()) {
                    map.insert(lines[i].trim().to_string(), (lines.get(i + 1).copied().unwrap_or("").to_string(), false));
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
        DbKind::Mariadb => {}
    }
    ps.iter().map(|x| map.get(x.name).map(|(v, pend)| Cur { value: display(kind, x, v), pending: *pend })).collect()
}

/// 校验并整理用户输入。空 = 恢复默认(返回 None);通过时返回规范写法
pub fn normalize(kind: DbKind, p: &Param, input: &str) -> Result<Option<String>, String> {
    let s = input.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let bad = |what: &str| Err(format!("「{}」{what}", p.label));
    match p.kind {
        PKind::Int(lo, hi) => match s.parse::<i64>() {
            Ok(n) if (lo..=hi).contains(&n) => Ok(Some(n.to_string())),
            _ => bad(&format!("要填 {lo} 到 {hi} 的整数")),
        },
        PKind::Dec => match s.parse::<f64>() {
            Ok(n) if n >= 0.0 && n < 31_536_000.0 && s.chars().all(|c| c.is_ascii_digit() || c == '.') => Ok(Some(s.to_string())),
            _ => bad("要填不小于 0 的数字,可以带小数"),
        },
        PKind::Size => {
            let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
            let unit = s[digits.len()..].trim().to_ascii_uppercase();
            let n: u64 = digits.parse().map_err(|_| format!("「{}」要填数字加单位,如 256M、2G", p.label))?;
            let mult: u64 = match unit.trim_end_matches('B') {
                "" if kind != DbKind::Postgres => 1,
                "K" => 1 << 10,
                "M" => 1 << 20,
                "G" => 1 << 30,
                "T" => 1 << 40,
                _ => return bad("要填数字加单位,如 256M、2G"),
            };
            let bytes = n.checked_mul(mult).ok_or_else(|| format!("「{}」太大", p.label))?;
            if bytes == 0 && !(kind == DbKind::Redis && p.name == "maxmemory") {
                return bad("不能是 0");
            }
            Ok(Some(match kind {
                DbKind::Postgres => format!("{n}{}", match unit.trim_end_matches('B') { "K" => "kB", "M" => "MB", "G" => "GB", _ => "TB" }),
                _ => human_bytes(bytes),
            }))
        }
        PKind::OnOff => match (kind, s.to_ascii_lowercase().as_str()) {
            (DbKind::Redis, "yes" | "on" | "true") => Ok(Some("yes".into())),
            (DbKind::Redis, "no" | "off" | "false") => Ok(Some("no".into())),
            (_, "on" | "yes" | "true" | "1") => Ok(Some("ON".into())),
            (_, "off" | "no" | "false" | "0") => Ok(Some("OFF".into())),
            _ => bad("要填 开 或 关"),
        },
        PKind::Enum(opts) => opts.iter().find(|o| o.eq_ignore_ascii_case(s)).map(|o| Some(o.to_string())).ok_or_else(|| format!("「{}」只能是:{}", p.label, opts.join("、"))),
        PKind::Zone => {
            let ok = s.eq_ignore_ascii_case("system") || {
                let b = s.as_bytes();
                matches!(b.first(), Some(b'+' | b'-')) && s[1..].split_once(':').is_some_and(|(h, m)| h.len() <= 2 && m.len() == 2 && h.chars().chain(m.chars()).all(|c| c.is_ascii_digit()))
            };
            if ok {
                Ok(Some(if s.eq_ignore_ascii_case("system") { "SYSTEM".into() } else { s.to_string() }))
            } else {
                bad("要填 +08:00 这样的偏移,或 SYSTEM")
            }
        }
    }
}

/// MySQL `SET PERSIST` 的右值
fn mysql_value(p: &Param, v: &str) -> String {
    match p.kind {
        PKind::Size => crate::dbparams::to_bytes(v).to_string(),
        PKind::Enum(_) | PKind::Zone => format!("'{v}'"),
        PKind::OnOff => v.to_string(),
        _ => v.to_string(),
    }
}

/// `128M` → 字节
pub fn to_bytes(v: &str) -> u64 {
    let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
    let n: u64 = digits.parse().unwrap_or(0);
    match v[digits.len()..].trim().to_ascii_uppercase().trim_end_matches('B') {
        "K" => n << 10,
        "M" => n << 20,
        "G" => n << 30,
        "T" => n << 40,
        _ => n,
    }
}

/// 一次修改:`(参数序号, 新值)`,新值 None 表示恢复默认
pub type Change = (usize, Option<String>);

/// 写入语句(stdin)。`changes` 里的值必须是 `normalize` 的结果
pub fn apply_sql(kind: DbKind, changes: &[Change]) -> Result<String, String> {
    let ps = params(kind);
    if changes.is_empty() {
        return Err("没有改动".into());
    }
    let mut s = String::new();
    for (i, v) in changes {
        let p = ps.get(*i).ok_or("参数序号不对")?;
        match kind {
            DbKind::Postgres => match v {
                Some(v) => s += &format!("ALTER SYSTEM SET {} = '{}';\n", p.name, v),
                None => s += &format!("ALTER SYSTEM RESET {};\n", p.name),
            },
            DbKind::Mysql => match v {
                Some(v) => s += &format!("SET PERSIST {} = {};\n", p.name, mysql_value(p, v)),
                None => s += &format!("SET PERSIST {} = DEFAULT;\n", p.name),
            },
            DbKind::Redis => {
                let v = v.clone().unwrap_or_else(|| p.default.to_string());
                // 容量用字节数写,避免 `128M` 在 Redis 里按 1000 进制理解
                let v = if p.kind == PKind::Size { to_bytes(&v).to_string() } else { v };
                s += &format!("CONFIG SET {} {}\n", p.name, if v.is_empty() { "\"\"".to_string() } else { v });
            }
            DbKind::Mariadb => return Err("MariaDB 暂不支持参数表".into()),
        }
    }
    match kind {
        DbKind::Postgres => s += "SELECT pg_reload_conf();\n",
        DbKind::Redis => s += "CONFIG REWRITE\n",
        _ => {}
    }
    Ok(s)
}

/// Redis 写入的结果:出错返回原因;`CONFIG REWRITE` 因为没有配置文件失败时,参数已在运行期生效,返回提示
pub fn redis_outcome(out: &str) -> Result<Option<String>, String> {
    let mut warn = None;
    for l in out.lines() {
        let l = l.trim();
        if l.contains("without a config file") {
            warn = Some("已生效,但这个 Redis 没有配置文件,重启后会恢复原来的值".to_string());
        } else if l.starts_with("ERR") || l.starts_with("(error)") {
            return Err(l.trim_start_matches("(error)").trim().to_string());
        }
    }
    Ok(warn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(kind: DbKind, name: &str) -> usize {
        params(kind).iter().position(|p| p.name == name).unwrap()
    }

    #[test]
    fn param_names_are_plain() {
        for k in [DbKind::Postgres, DbKind::Mysql, DbKind::Redis] {
            for p in params(k) {
                assert!(p.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-')), "{}", p.name);
                // 默认值必须是合法输入
                assert!(normalize(k, p, p.default).is_ok(), "{} 默认值 {}", p.name, p.default);
            }
        }
        assert!(!supported(DbKind::Mariadb) && params(DbKind::Mariadb).is_empty());
    }

    #[test]
    fn normalize_values() {
        let pg = |n: &str, v: &str| normalize(DbKind::Postgres, &PG[idx(DbKind::Postgres, n)], v);
        assert_eq!(pg("shared_buffers", "256mb"), Ok(Some("256MB".into())));
        assert_eq!(pg("shared_buffers", "1g"), Ok(Some("1GB".into())));
        assert_eq!(pg("shared_buffers", "512 kB"), Ok(Some("512kB".into())));
        assert!(pg("shared_buffers", "256").is_err(), "PostgreSQL 容量要带单位");
        assert!(pg("shared_buffers", "x").is_err() && pg("shared_buffers", "0MB").is_err());
        assert_eq!(pg("max_connections", " 200 "), Ok(Some("200".into())));
        assert!(pg("max_connections", "0").is_err() && pg("max_connections", "1; DROP").is_err());
        assert_eq!(pg("log_min_duration_statement", "-1"), Ok(Some("-1".into())));
        assert_eq!(pg("work_mem", ""), Ok(None));

        let my = |n: &str, v: &str| normalize(DbKind::Mysql, &MYSQL[idx(DbKind::Mysql, n)], v);
        assert_eq!(my("innodb_buffer_pool_size", "512m"), Ok(Some("512M".into())));
        assert_eq!(my("innodb_buffer_pool_size", "1024M"), Ok(Some("1G".into())));
        assert_eq!(my("innodb_buffer_pool_size", "1048576"), Ok(Some("1M".into())));
        assert_eq!(my("slow_query_log", "on"), Ok(Some("ON".into())));
        assert!(my("slow_query_log", "maybe").is_err());
        assert_eq!(my("long_query_time", "0.5"), Ok(Some("0.5".into())));
        assert!(my("long_query_time", "1e3").is_err() && my("long_query_time", "-1").is_err());
        assert_eq!(my("character_set_server", "UTF8MB4"), Ok(Some("utf8mb4".into())));
        assert!(my("character_set_server", "x'; --").is_err());
        assert_eq!(my("time_zone", "+08:00"), Ok(Some("+08:00".into())));
        assert_eq!(my("time_zone", "system"), Ok(Some("SYSTEM".into())));
        assert!(my("time_zone", "Asia/Shanghai").is_err() && my("time_zone", "+8").is_err());

        let rd = |n: &str, v: &str| normalize(DbKind::Redis, &REDIS[idx(DbKind::Redis, n)], v);
        assert_eq!(rd("maxmemory", "256mb"), Ok(Some("256M".into())));
        assert_eq!(rd("maxmemory", "0"), Ok(Some("0".into())));
        assert_eq!(rd("appendonly", "on"), Ok(Some("yes".into())));
        assert_eq!(rd("maxmemory-policy", "ALLKEYS-LRU"), Ok(Some("allkeys-lru".into())));
    }

    #[test]
    fn parse_pg_mysql_redis() {
        let pg = parse_current(DbKind::Postgres, "max_connections\t100\tf\nshared_buffers\t128MB\tt\nwork_mem\t4MB\tf\n");
        assert_eq!(pg[0], Some(Cur { value: "100".into(), pending: false }));
        assert_eq!(pg[1], Some(Cur { value: "128MB".into(), pending: true }));
        assert_eq!(pg.len(), PG.len());
        assert!(pg[3].is_none());

        let my = parse_current(DbKind::Mysql, "max_connections\t151\ninnodb_buffer_pool_size\t134217728\nslow_query_log\t0\nlong_query_time\t10.000000\n");
        assert_eq!(my[0].as_ref().unwrap().value, "151");
        assert_eq!(my[1].as_ref().unwrap().value, "128M");
        assert_eq!(my[idx(DbKind::Mysql, "slow_query_log")].as_ref().unwrap().value, "OFF");

        let rd = parse_current(DbKind::Redis, "maxmemory\n268435456\nmaxmemory-policy\nnoeviction\nappendonly\nno\ntimeout\n0\n");
        assert_eq!(rd[0].as_ref().unwrap().value, "256M");
        assert_eq!(rd[1].as_ref().unwrap().value, "noeviction");
        assert_eq!(rd[2].as_ref().unwrap().value, "no");
        assert_eq!(rd[3].as_ref().unwrap().value, "0");
    }

    #[test]
    fn apply_statements() {
        let pg = apply_sql(DbKind::Postgres, &[(idx(DbKind::Postgres, "shared_buffers"), Some("256MB".into())), (idx(DbKind::Postgres, "work_mem"), None)]).unwrap();
        assert_eq!(pg, "ALTER SYSTEM SET shared_buffers = '256MB';\nALTER SYSTEM RESET work_mem;\nSELECT pg_reload_conf();\n");
        let my = apply_sql(DbKind::Mysql, &[(idx(DbKind::Mysql, "innodb_buffer_pool_size"), Some("1G".into())), (idx(DbKind::Mysql, "character_set_server"), Some("utf8mb4".into())), (idx(DbKind::Mysql, "slow_query_log"), Some("ON".into())), (idx(DbKind::Mysql, "wait_timeout"), None)]).unwrap();
        assert_eq!(my, "SET PERSIST innodb_buffer_pool_size = 1073741824;\nSET PERSIST character_set_server = 'utf8mb4';\nSET PERSIST slow_query_log = ON;\nSET PERSIST wait_timeout = DEFAULT;\n");
        let rd = apply_sql(DbKind::Redis, &[(idx(DbKind::Redis, "maxmemory"), Some("256M".into())), (idx(DbKind::Redis, "appendonly"), None)]).unwrap();
        assert_eq!(rd, "CONFIG SET maxmemory 268435456\nCONFIG SET appendonly no\nCONFIG REWRITE\n");
        assert!(apply_sql(DbKind::Postgres, &[]).is_err());
        assert!(apply_sql(DbKind::Mariadb, &[(0, None)]).is_err());
    }

    #[test]
    fn redis_results() {
        assert_eq!(redis_outcome("OK\nOK\nOK\n"), Ok(None));
        assert!(redis_outcome("OK\nERR The server is running without a config file\n").unwrap().is_some());
        assert_eq!(redis_outcome("ERR Invalid argument 'x' for CONFIG SET 'maxmemory'\n"), Err("ERR Invalid argument 'x' for CONFIG SET 'maxmemory'".into()));
    }

    #[test]
    fn read_sql_lists_every_param() {
        for k in [DbKind::Postgres, DbKind::Mysql, DbKind::Redis] {
            let s = read_sql(k);
            for p in params(k) {
                assert!(s.contains(p.name), "{} 缺 {}", k.label(), p.name);
            }
        }
    }
}
