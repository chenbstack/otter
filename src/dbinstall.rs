//! 安装数据库:根据表单生成 compose 文本和 `.env`。部署走 `Action::ComposeCreate`(新建堆栈的同一条路)。
//!
//! 密码只写进 `.env`(`DB_PASSWORD=`),compose 文件里只有 `${DB_PASSWORD}`;Redis 的密码经环境变量交给
//! 启动脚本,不出现在进程参数里。这里全是纯函数,不碰网络和界面。

use crate::dbinst::DbKind;
use rand::RngExt as _;

pub const KINDS: [DbKind; 4] = [DbKind::Postgres, DbKind::Mysql, DbKind::Mariadb, DbKind::Redis];

/// 自定义版本号:镜像 tag 的合法写法
pub fn valid_tag(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphanumeric()) && s.len() <= 128 && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn image(kind: DbKind, version: &str) -> String {
    let name = match kind {
        DbKind::Postgres => "postgres",
        DbKind::Mysql => "mysql",
        DbKind::Mariadb => "mariadb",
        DbKind::Redis => "redis",
    };
    format!("{name}:{version}")
}

fn data_dir(kind: DbKind, version: &str) -> &'static str {
    match kind {
        // PostgreSQL 18 起镜像把数据放在 /var/lib/postgresql 下的版本子目录,挂载点也跟着变
        DbKind::Postgres if version.split(['.', '-']).next().and_then(|m| m.parse::<u32>().ok()).is_some_and(|m| m >= 18) => "/var/lib/postgresql",
        DbKind::Postgres => "/var/lib/postgresql/data",
        DbKind::Mysql | DbKind::Mariadb => "/var/lib/mysql",
        DbKind::Redis => "/data",
    }
}

fn password_env(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => "POSTGRES_PASSWORD",
        DbKind::Mysql => "MYSQL_ROOT_PASSWORD",
        DbKind::Mariadb => "MARIADB_ROOT_PASSWORD",
        DbKind::Redis => "REDIS_PASSWORD",
    }
}

/// 24 位强随机密码:字母数字加 `-_`(约 142 位熵),系统随机数。不含 compose/.env 里有特殊含义的字符
pub fn gen_password() -> String {
    const CS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut rng = rand::rng();
    (0..24).map(|_| CS[rng.random_range(0..CS.len())] as char).collect()
}

/// CPU 核数:正数,最多两位小数,不超过 1024;空表示不限制
pub fn valid_cpus(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Ok(());
    }
    let ok = s.split_once('.').map_or(s.chars().all(|c| c.is_ascii_digit()), |(a, b)| !a.is_empty() && !b.is_empty() && b.len() <= 2 && a.chars().chain(b.chars()).all(|c| c.is_ascii_digit()));
    match s.parse::<f64>() {
        Ok(v) if ok && v >= 0.01 && v <= 1024.0 => Ok(()),
        _ => Err("CPU 限制填核数,如 0.5、2(最多两位小数);不限制就留空".into()),
    }
}

/// 内存:整数加单位 m / g(如 512m、2g),至少 4m;空表示不限制
pub fn valid_memory(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Ok(());
    }
    let bad = || Err("内存限制填整数,如 512、2;不限制就留空".to_string());
    let Some(unit) = s.chars().last().map(|c| c.to_ascii_lowercase()).filter(|c| matches!(c, 'm' | 'g')) else { return bad() };
    let num = &s[..s.len() - 1];
    let Some(n) = (!num.is_empty() && num.len() <= 6 && num.chars().all(|c| c.is_ascii_digit())).then(|| num.parse::<u64>().ok()).flatten() else { return bad() };
    if unit == 'm' && n < 4 || n == 0 {
        return Err("内存限制至少 4m".into());
    }
    Ok(())
}

/// 自己输入的密码:不含 compose/.env 会解析出歧义的字符
pub fn valid_password(s: &str) -> Result<(), String> {
    if s.len() < 8 {
        return Err("密码至少 8 位".into());
    }
    if s.len() > 128 {
        return Err("密码太长(最多 128 位)".into());
    }
    if s.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '$' | '\'' | '"' | '`' | '\\' | '#')) {
        return Err("密码里不能有空格、引号、$、\\、# 和控制字符".into());
    }
    Ok(())
}

/// 自动生成的名字:`postgres-a3f9`,避开已有的堆栈名和容器名
pub fn gen_name(kind: DbKind, taken: &[String]) -> String {
    let base = image(kind, "").trim_end_matches(':').to_string();
    let mut rng = rand::rng();
    loop {
        let suf: String = (0..4).map(|_| b"0123456789abcdef"[rng.random_range(0..16)] as char).collect();
        let n = format!("{base}-{suf}");
        if !taken.contains(&n) {
            return n;
        }
    }
}

pub struct Spec<'a> {
    pub kind: DbKind,
    pub version: &'a str,
    pub project: &'a str,
    pub container: &'a str,
    /// 宿主机上的端口
    pub port: u16,
    /// 对外开放:不绑 127.0.0.1
    pub public: bool,
    pub password: &'a str,
    /// CPU 核数上限,如 `1.5`;空 = 不限制
    pub cpus: &'a str,
    /// 内存上限,如 `512m`、`2g`;空 = 不限制
    pub memory: &'a str,
}

impl Spec<'_> {
    /// 检查表单;`taken_names` 是已有的堆栈名和容器名,`used_ports` 是服务器上已被占用的宿主机端口
    pub fn validate(&self, taken_names: &[String], used_ports: &[u16]) -> Result<(), String> {
        if !valid_tag(self.version) {
            return Err("版本号只能用字母、数字、. _ -".into());
        }
        for (what, n) in [("堆栈名", self.project), ("容器名", self.container)] {
            if !crate::docker::valid_project(n) {
                return Err(format!("{what}只能用小写字母、数字、- 和 _,并以字母或数字开头"));
            }
        }
        if taken_names.iter().any(|n| n == self.project) {
            return Err("已经有同名的堆栈".into());
        }
        if taken_names.iter().any(|n| n == self.container) {
            return Err("已经有同名的容器".into());
        }
        if self.port < 1024 {
            return Err("端口要在 1024 到 65535 之间".into());
        }
        if used_ports.contains(&self.port) {
            return Err(format!("服务器上 {} 端口已被占用,换一个", self.port));
        }
        valid_cpus(self.cpus)?;
        valid_memory(self.memory)?;
        valid_password(self.password)
    }

    /// `docker-compose.yml` 文本。密码只以 `${DB_PASSWORD}` 出现
    pub fn compose(&self) -> String {
        let k = self.kind;
        let bind = if self.public { String::new() } else { "127.0.0.1:".into() };
        let mut s = String::new();
        s += "services:\n  db:\n";
        s += &format!("    image: {}\n", image(k, self.version));
        s += &format!("    container_name: {}\n", self.container);
        s += "    restart: unless-stopped\n";
        if !self.cpus.is_empty() {
            s += &format!("    cpus: \"{}\"\n", self.cpus);
        }
        if !self.memory.is_empty() {
            s += &format!("    mem_limit: {}\n", self.memory.to_ascii_lowercase());
        }
        s += &format!("    ports:\n      - \"{bind}{}:{}\"\n", self.port, k.port());
        s += &format!("    volumes:\n      - {}-data:{}\n", self.container, data_dir(k, self.version));
        s += &format!("    environment:\n      {}: ${{DB_PASSWORD}}\n", password_env(k));
        if k == DbKind::Redis {
            s += "    command: [\"sh\", \"-c\", \"exec redis-server --appendonly yes --requirepass \\\"$$REDIS_PASSWORD\\\"\"]\n";
        }
        s += "    labels:\n      otter.managed: db\n";
        s += &format!("volumes:\n  {}-data:\n", self.container);
        s
    }

    /// `.env` 文本(含密码)
    pub fn env(&self) -> String {
        format!("DB_PASSWORD={}\n", self.password)
    }

    /// 给确认框预览用:`.env` 里的密码打码
    pub fn env_masked(&self) -> String {
        "DB_PASSWORD=••••••••\n".into()
    }
}

/// 查 Docker Hub 上官方镜像的 tag 的地址
pub fn tags_url(kind: DbKind) -> String {
    format!("https://hub.docker.com/v2/repositories/library/{}/tags?page_size=100&ordering=last_updated", image(kind, "").trim_end_matches(':'))
}

/// 服务器上取 tag 列表的脚本:curl,没有就 wget;输出 JSON
pub fn tags_script(kind: DbKind) -> String {
    let u = tags_url(kind);
    format!("(curl -fsSL --max-time 12 '{u}' || wget -qO- -T 12 '{u}') 2>&1")
}

/// 版本号 `17.5`/`8.4.5` → 可排序的数字元组;不是纯数字版本返回 None
fn ver_key(tag: &str) -> Option<Vec<u32>> {
    let parts: Vec<u32> = tag.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    (1..=3).contains(&parts.len()).then_some(parts)
}

/// 从 Docker Hub 的 tags 接口响应里挑出稳定版本号(纯数字,不带 -alpine 等后缀),新的在前,最多 60 个。解析不出来返回 None
pub fn parse_tags(json: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut tags: Vec<(Vec<u32>, String)> = v.get("results")?.as_array()?.iter().filter_map(|r| r.get("name")?.as_str()).filter_map(|n| ver_key(n).map(|k| (k, n.to_string()))).collect();
    tags.sort_by(|a, b| b.0.cmp(&a.0));
    tags.dedup_by(|a, b| a.1 == b.1);
    let list: Vec<String> = tags.into_iter().take(60).map(|t| t.1).collect();
    (!list.is_empty()).then_some(list)
}

/// 本机直接请求 Docker Hub(阻塞,放后台线程调用)
pub fn fetch_tags_local(kind: DbKind) -> Result<Vec<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(12))).user_agent("otter").build().into();
    let mut resp = agent.get(&tags_url(kind)).call().map_err(|e| format!("请求 Docker Hub 失败:{e}"))?;
    let body = resp.body_mut().read_to_string().map_err(|e| format!("读取响应失败:{e}"))?;
    parse_tags(&body).ok_or_else(|| "Docker Hub 返回的内容里没有版本".to_string())
}

/// 容器列表里已占用的宿主机端口(`0.0.0.0:5432->5432/tcp`、`:::80->80/tcp`、`127.0.0.1:1000-1002->...`)
pub fn host_ports(ports: &[String]) -> Vec<u16> {
    let mut v = Vec::new();
    for p in ports {
        let Some((left, _)) = p.split_once("->") else { continue };
        let range = left.rsplit(':').next().unwrap_or(left);
        let (a, b) = range.split_once('-').unwrap_or((range, range));
        if let (Ok(a), Ok(b)) = (a.parse::<u16>(), b.parse::<u16>()) {
            v.extend(a..=b.min(a.saturating_add(64)));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: DbKind, public: bool) -> Spec<'static> {
        Spec { kind, version: "17", project: "pg-main", container: "pg-main", port: 15432, public, password: "abcDEF123-_xyz", cpus: "", memory: "" }
    }

    #[test]
    fn dump_composes_for_real_node() {
        let Ok(dir) = std::env::var("OTTER_DUMP_SCRIPTS") else { return };
        let vers = [(DbKind::Postgres, "16", 15441u16), (DbKind::Mysql, "8.4", 15442), (DbKind::Mariadb, "11", 15443), (DbKind::Redis, "7", 15444)];
        for (k, v, port) in vers {
            let name = format!("otgi-{}", k.label().to_lowercase());
            let s = Spec { kind: k, version: v, project: &name, container: &name, port, public: false, password: "abcDEF123xyz", cpus: "", memory: "" };
            std::fs::write(format!("{dir}/{name}.yml"), s.compose()).unwrap();
            std::fs::write(format!("{dir}/{name}.env"), s.env()).unwrap();
        }
    }

    #[test]
    fn password_never_appears_in_compose() {
        for k in KINDS {
            let s = spec(k, false);
            assert!(!s.compose().contains(s.password));
            assert!(s.compose().contains("${DB_PASSWORD}"));
            assert_eq!(s.env(), "DB_PASSWORD=abcDEF123-_xyz\n");
            assert!(!s.env_masked().contains(s.password));
        }
    }

    #[test]
    fn ports_bind_to_loopback_unless_public() {
        assert!(spec(DbKind::Postgres, false).compose().contains("\"127.0.0.1:15432:5432\""));
        let p = spec(DbKind::Postgres, true).compose();
        assert!(p.contains("\"15432:5432\"") && !p.contains("127.0.0.1"));
    }

    #[test]
    fn compose_per_kind() {
        let m = spec(DbKind::Mysql, false).compose();
        assert!(m.contains("image: mysql:17") && m.contains("MYSQL_ROOT_PASSWORD: ${DB_PASSWORD}") && m.contains("pg-main-data:/var/lib/mysql"));
        let r = spec(DbKind::Redis, false).compose();
        assert!(r.contains("REDIS_PASSWORD: ${DB_PASSWORD}") && r.contains("$$REDIS_PASSWORD") && r.contains(":6379\""));
        let p = spec(DbKind::Postgres, false).compose();
        assert!(p.contains("otter.managed: db") && p.contains("container_name: pg-main") && p.contains("restart: unless-stopped"));
        assert!(p.ends_with("volumes:\n  pg-main-data:\n"));
    }

    #[test]
    fn generated_password_is_strong_and_valid() {
        let a = gen_password();
        assert_eq!(a.len(), 24);
        assert!(valid_password(&a).is_ok());
        assert_ne!(a, gen_password());
    }

    #[test]
    fn custom_versions() {
        assert!(valid_tag("17.5-alpine") && valid_tag("8.0.42") && !valid_tag("") && !valid_tag("a b") && !valid_tag("-x") && !valid_tag("a:b"));
        let mut s = spec(DbKind::Postgres, false);
        s.version = "18.1";
        assert!(s.compose().contains("pg-main-data:/var/lib/postgresql\n"));
        s.version = "16.9";
        assert!(s.compose().contains("/var/lib/postgresql/data\n"));
        s.version = "17; rm";
        assert!(s.validate(&[], &[]).is_err());
    }

    #[test]
    fn resource_limits() {
        for ok in ["", "0.5", "2", "1.25", "16"] {
            assert!(valid_cpus(ok).is_ok(), "{ok}");
        }
        for bad in ["0", "0.001", "-1", "1.", ".5", "abc", "2000", "1e2", "1 2"] {
            assert!(valid_cpus(bad).is_err(), "{bad}");
        }
        for ok in ["", "512m", "2g", "2G", "4m"] {
            assert!(valid_memory(ok).is_ok(), "{ok}");
        }
        for bad in ["512", "m", "0g", "3m", "1.5g", "2gb", "-1g", "1234567m"] {
            assert!(valid_memory(bad).is_err(), "{bad}");
        }
        let mut s = spec(DbKind::Postgres, false);
        assert!(!s.compose().contains("cpus") && !s.compose().contains("mem_limit"));
        s.cpus = "1.5";
        s.memory = "2G";
        let c = s.compose();
        assert!(c.contains("    cpus: \"1.5\"\n") && c.contains("    mem_limit: 2g\n"));
        s.memory = "2x";
        assert!(s.validate(&[], &[]).is_err());
    }

    #[test]
    fn password_rules() {
        assert!(valid_password("short").is_err());
        for bad in ["abcd efgh1", "abcdefgh$1", "abcdefgh'1", "abcdefgh\"1", "abcdefgh\\1", "abcdefgh#1", "abcdefgh\n1"] {
            assert!(valid_password(bad).is_err(), "{bad}");
        }
        assert!(valid_password("Abcdef12-_.@%+=").is_ok());
    }

    #[test]
    fn names_avoid_taken_and_validate() {
        let n = gen_name(DbKind::Postgres, &[]);
        assert!(n.starts_with("postgres-") && crate::docker::valid_project(&n));
        let taken = vec!["pg-main".to_string()];
        let mut s = spec(DbKind::Postgres, false);
        assert!(s.validate(&taken, &[]).unwrap_err().contains("同名的堆栈"));
        s.project = "pg-2";
        assert!(s.validate(&taken, &[]).unwrap_err().contains("同名的容器"));
        s.container = "pg-2";
        assert!(s.validate(&taken, &[15432]).unwrap_err().contains("已被占用"));
        assert!(s.validate(&taken, &[]).is_ok());
        s.container = "Bad Name";
        assert!(s.validate(&taken, &[]).is_err());
    }

    #[test]
    fn parses_host_ports() {
        let v = host_ports(&["0.0.0.0:5432->5432/tcp".into(), ":::5432->5432/tcp".into(), "127.0.0.1:7000-7002->7000-7002/tcp".into(), "3306/tcp".into()]);
        assert_eq!(v, vec![5432, 5432, 7000, 7001, 7002]);
    }

    #[test]
    fn picks_stable_tags_newest_first() {
        let j = r#"{"results":[{"name":"17.5-alpine"},{"name":"16.9"},{"name":"17"},{"name":"17.5"},{"name":"latest"},{"name":"18.0"},{"name":"16"},{"name":"17.5"},{"name":"bookworm"},{"name":"9.4.12"}]}"#;
        assert_eq!(parse_tags(j).unwrap(), ["18.0", "17.5", "17", "16.9", "16", "9.4.12"]);
        assert!(parse_tags("not json").is_none());
        assert!(parse_tags(r#"{"results":[{"name":"latest"}]}"#).is_none());
        assert!(tags_url(DbKind::Mariadb).contains("/library/mariadb/tags"));
    }
}
