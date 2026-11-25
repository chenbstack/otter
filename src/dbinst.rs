//! 数据库实例识别:从 Docker 容器列表里找出 PostgreSQL、MySQL、MariaDB、Redis 容器。
//! 只看镜像名,不区分是谁创建的(Otter、compose、面板、手敲 `docker run` 都算)。

use crate::docker::Container;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbKind {
    Postgres,
    Mysql,
    Mariadb,
    Redis,
}

impl DbKind {
    pub fn label(self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::Mysql => "MySQL",
            DbKind::Mariadb => "MariaDB",
            DbKind::Redis => "Redis",
        }
    }

    /// 容器里的默认端口
    pub fn port(self) -> u16 {
        match self {
            DbKind::Postgres => 5432,
            DbKind::Mysql | DbKind::Mariadb => 3306,
            DbKind::Redis => 6379,
        }
    }
}

/// 镜像引用拆成(去掉仓库地址和路径后的镜像名, tag)。`registry:5000/lib/postgres:16@sha256:..` → (`postgres`, `16`)
fn split_image(image: &str) -> (&str, &str) {
    let no_digest = image.split('@').next().unwrap_or(image);
    let last = no_digest.rsplit('/').next().unwrap_or(no_digest);
    match last.split_once(':') {
        Some((n, t)) => (n, t),
        None => (last, "latest"),
    }
}

pub fn kind_of_image(image: &str) -> Option<DbKind> {
    let (name, _) = split_image(image);
    match name.to_ascii_lowercase().as_str() {
        // postgis、pgvector、timescaledb 都是带扩展的 PostgreSQL 镜像
        "postgres" | "postgresql" | "postgis" | "pgvector" | "timescaledb" => Some(DbKind::Postgres),
        "mysql" | "mysql-server" => Some(DbKind::Mysql),
        "mariadb" => Some(DbKind::Mariadb),
        "redis" | "redis-server" => Some(DbKind::Redis),
        _ => None,
    }
}

/// 一个被识别出来的数据库容器
#[derive(Clone, Debug)]
pub struct DbInstance {
    pub kind: DbKind,
    pub version: String,
    pub container: Container,
}

pub fn detect(containers: &[Container]) -> Vec<DbInstance> {
    let mut v: Vec<DbInstance> = containers
        .iter()
        .filter_map(|c| {
            let kind = kind_of_image(&c.image)?;
            Some(DbInstance { kind, version: split_image(&c.image).1.to_string(), container: c.clone() })
        })
        .collect();
    // 运行中的在前,再按名字
    v.sort_by(|a, b| b.container.running().cmp(&a.container.running()).then_with(|| a.container.name.cmp(&b.container.name)));
    v
}

// ---- 容器里的命令和 SQL ----
//
// 所有命令都是 `docker exec -i <容器> sh -c <固定脚本>`,SQL 从 stdin 传入。脚本在容器里用容器自己的环境变量登录
// (`POSTGRES_PASSWORD`、`MYSQL_ROOT_PASSWORD` 等),密码交给客户端的环境变量,不经过 Otter、不进命令行。
// 用户输入只会出现在 SQL 里:库名、用户名、主机严格校验;密码按字符串字面量转义。

/// 库名、用户名:字母数字下划线 `$`,字母或下划线开头。比各数据库实际允许的窄,换来拼进 SQL 时不用再想引号问题。
pub fn valid_ident(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && s.len() <= 63 && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'))
}

/// MySQL 账号的主机部分:`%`、`localhost`、IP、带通配符的网段
pub fn valid_host(s: &str) -> bool {
    !s.is_empty() && s.len() <= 60 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '%' | ':' | '-' | '_'))
}

pub fn valid_password(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("密码不能为空".into());
    }
    if s.len() > 128 {
        return Err("密码太长(最多 128 个字符)".into());
    }
    if s.chars().any(|c| c.is_control()) {
        return Err("密码里不能有换行等控制字符".into());
    }
    Ok(())
}

/// PostgreSQL 的字符串字面量:单引号翻倍(`standard_conforming_strings` 默认开,反斜杠不是转义)
fn pg_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// MySQL/MariaDB 的字符串字面量:单引号翻倍、反斜杠加倍(在 `NO_BACKSLASH_ESCAPES` 下只会让密码多一个反斜杠,不会逃出引号)
fn my_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

impl DbKind {
    /// 容器里读 stdin 并执行的客户端脚本;输出无表头、制表符分隔
    fn client_script(self) -> &'static str {
        match self {
            DbKind::Postgres => {
                r#"u="${POSTGRES_USER:-${POSTGRESQL_USERNAME:-postgres}}"; p="${POSTGRES_PASSWORD:-${POSTGRESQL_PASSWORD:-${POSTGRESQL_POSTGRES_PASSWORD:-}}}"; [ -n "$p" ] && export PGPASSWORD="$p"; exec psql -X -q -A -t -F "$(printf '\t')" -v ON_ERROR_STOP=1 -U "$u" -d postgres"#
            }
            DbKind::Mysql | DbKind::Mariadb => {
                r#"p="${MYSQL_ROOT_PASSWORD:-${MARIADB_ROOT_PASSWORD:-}}"; [ -n "$p" ] && export MYSQL_PWD="$p"; c=$(command -v mariadb || command -v mysql) || { echo "容器里没有 mysql 客户端" >&2; exit 1; }; exec "$c" -uroot -B -N"#
            }
            DbKind::Redis => r#"[ -n "$REDIS_PASSWORD" ] && export REDISCLI_AUTH="$REDIS_PASSWORD"; exec redis-cli"#,
        }
    }

    /// `docker` 的参数:`exec -i <容器> sh -c <脚本>`
    pub fn exec_args(self, container: &str) -> Vec<String> {
        ["exec", "-i", container, "sh", "-c", self.client_script()].map(String::from).to_vec()
    }

    /// 读管理员账号名的脚本,取自容器自己的环境变量;只读用户名,不碰密码
    pub fn admin_args(self, container: &str) -> Option<Vec<String>> {
        let script = match self {
            DbKind::Postgres => r#"printf '%s' "${POSTGRES_USER:-postgres}""#,
            DbKind::Mysql | DbKind::Mariadb => r#"printf 'root'"#,
            DbKind::Redis => return None,
        };
        Some(["exec", container, "sh", "-c", script].map(String::from).to_vec())
    }

    pub fn is_sql(self) -> bool {
        self != DbKind::Redis
    }

    pub fn list_dbs_sql(self) -> &'static str {
        match self {
            DbKind::Postgres => "SELECT d.datname, pg_get_userbyid(d.datdba), pg_encoding_to_char(d.encoding), (SELECT count(*) FROM pg_stat_activity a WHERE a.datname = d.datname), pg_database_size(d.datname), d.datcollate, regexp_replace(COALESCE(shobj_description(d.oid, 'pg_database'), ''), '[\t\r\n]+', ' ', 'g') FROM pg_database d WHERE NOT d.datistemplate ORDER BY 1;\n",
            DbKind::Mysql | DbKind::Mariadb => "SELECT s.schema_name, s.default_character_set_name, (SELECT COUNT(*) FROM information_schema.processlist p WHERE p.db = s.schema_name), COALESCE(SUM(t.data_length + t.index_length), 0), s.default_collation_name, COUNT(t.table_name) FROM information_schema.schemata s LEFT JOIN information_schema.tables t ON t.table_schema = s.schema_name GROUP BY s.schema_name, s.default_character_set_name, s.default_collation_name ORDER BY 1;\n",
            DbKind::Redis => "",
        }
    }

    pub fn list_users_sql(self) -> &'static str {
        match self {
            DbKind::Postgres => "SELECT rolname, rolsuper, rolcanlogin, rolcreatedb, rolname = current_user FROM pg_roles WHERE rolname !~ '^pg_' ORDER BY 1;\n",
            DbKind::Mysql | DbKind::Mariadb => "SELECT user, host FROM mysql.user ORDER BY 1, 2;\n",
            DbKind::Redis => "",
        }
    }

    pub fn create_db_sql(self, name: &str, owner: &str) -> Result<String, String> {
        if !valid_ident(name) {
            return Err("库名只能用字母、数字、下划线和 $,以字母或下划线开头".into());
        }
        if !owner.is_empty() && !valid_ident(owner) {
            return Err("用户名只能用字母、数字、下划线和 $,以字母或下划线开头".into());
        }
        Ok(match self {
            DbKind::Postgres if owner.is_empty() => format!("CREATE DATABASE \"{name}\";\n"),
            DbKind::Postgres => format!("CREATE DATABASE \"{name}\" OWNER \"{owner}\";\n"),
            DbKind::Mysql | DbKind::Mariadb => {
                let mut q = format!("CREATE DATABASE `{name}` CHARACTER SET utf8mb4;\n");
                if !owner.is_empty() {
                    q += &format!("GRANT ALL PRIVILEGES ON `{name}`.* TO '{owner}'@'%';\n");
                }
                q
            }
            DbKind::Redis => return Err("Redis 没有库的概念".into()),
        })
    }

    /// 新建「库 + 它的用户」。`full` = 该用户拥有这个库的最高权限(PostgreSQL 里是库的所有者,MySQL 里是该库全部权限并可授权),
    /// 否则只能读写数据、不能改表结构。权限都只限这一个库,不涉及其他库。`comment` 只有 PostgreSQL 支持(写成库注释)
    pub fn create_db_with_user_sql(self, db: &str, user: &str, password: &str, full: bool, comment: &str) -> Result<String, String> {
        if !valid_ident(db) {
            return Err("库名只能用字母、数字、下划线和 $,以字母或下划线开头".into());
        }
        if comment.chars().any(|c| c.is_control() && c != '\n') || comment.chars().count() > 500 {
            return Err("描述最多 500 个字".into());
        }
        let mut q = self.create_user_sql(user, "%", password, "")?;
        match self {
            DbKind::Postgres if full => {
                q += &format!("CREATE DATABASE \"{db}\" OWNER \"{user}\";\n");
                if !comment.is_empty() {
                    q += &format!("COMMENT ON DATABASE \"{db}\" IS {};\n", pg_str(comment));
                }
            }
            DbKind::Postgres => {
                q += &format!("CREATE DATABASE \"{db}\";\n");
                if !comment.is_empty() {
                    q += &format!("COMMENT ON DATABASE \"{db}\" IS {};\n", pg_str(comment));
                }
                q += &format!("GRANT CONNECT ON DATABASE \"{db}\" TO \"{user}\";\n");
                // 表和序列在库里面授权:连进这个库再授(psql 的 \c 沿用 PGPASSWORD)
                q += &format!("\\c \"{db}\"\n");
                q += &format!("GRANT USAGE ON SCHEMA public TO \"{user}\";\n");
                q += &format!("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO \"{user}\";\n");
                q += &format!("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO \"{user}\";\n");
                q += &format!("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO \"{user}\";\n");
            }
            DbKind::Mysql | DbKind::Mariadb => {
                q += &format!("CREATE DATABASE `{db}` CHARACTER SET utf8mb4;\n");
                if full {
                    q += &format!("GRANT ALL PRIVILEGES ON `{db}`.* TO '{user}'@'%' WITH GRANT OPTION;\n");
                } else {
                    q += &format!("GRANT SELECT, INSERT, UPDATE, DELETE ON `{db}`.* TO '{user}'@'%';\n");
                }
            }
            DbKind::Redis => return Err("Redis 没有库的概念".into()),
        }
        Ok(q)
    }

    pub fn drop_db_sql(self, name: &str) -> Result<String, String> {
        if !valid_ident(name) {
            return Err("库名里有不支持的字符,请到终端里处理".into());
        }
        Ok(match self {
            DbKind::Postgres => format!("DROP DATABASE \"{name}\";\n"),
            DbKind::Mysql | DbKind::Mariadb => format!("DROP DATABASE `{name}`;\n"),
            DbKind::Redis => return Err("Redis 没有库的概念".into()),
        })
    }

    /// 建用户;`host` 只对 MySQL/MariaDB 有意义(默认 `%`);`grant_db` 非空时授予该库的全部权限
    pub fn create_user_sql(self, name: &str, host: &str, password: &str, grant_db: &str) -> Result<String, String> {
        if !valid_ident(name) {
            return Err("用户名只能用字母、数字、下划线和 $,以字母或下划线开头".into());
        }
        valid_password(password)?;
        if !grant_db.is_empty() && !valid_ident(grant_db) {
            return Err("库名只能用字母、数字、下划线和 $,以字母或下划线开头".into());
        }
        Ok(match self {
            DbKind::Postgres => {
                let mut q = format!("CREATE ROLE \"{name}\" LOGIN PASSWORD {};\n", pg_str(password));
                if !grant_db.is_empty() {
                    q += &format!("GRANT ALL PRIVILEGES ON DATABASE \"{grant_db}\" TO \"{name}\";\n");
                }
                q
            }
            DbKind::Mysql | DbKind::Mariadb => {
                let host = if host.is_empty() { "%" } else { host };
                if !valid_host(host) {
                    return Err("主机只能用字母、数字和 . % : - _".into());
                }
                let mut q = format!("CREATE USER '{name}'@'{host}' IDENTIFIED BY {};\n", my_str(password));
                if !grant_db.is_empty() {
                    q += &format!("GRANT ALL PRIVILEGES ON `{grant_db}`.* TO '{name}'@'{host}';\n");
                }
                q
            }
            DbKind::Redis => return Err("Redis 不在这里管理用户".into()),
        })
    }

    pub fn alter_password_sql(self, name: &str, host: &str, password: &str) -> Result<String, String> {
        if !valid_ident(name) {
            return Err("用户名里有不支持的字符,请到终端里处理".into());
        }
        valid_password(password)?;
        Ok(match self {
            DbKind::Postgres => format!("ALTER ROLE \"{name}\" WITH PASSWORD {};\n", pg_str(password)),
            DbKind::Mysql | DbKind::Mariadb => {
                if !valid_host(host) {
                    return Err("主机里有不支持的字符,请到终端里处理".into());
                }
                format!("ALTER USER '{name}'@'{host}' IDENTIFIED BY {};\n", my_str(password))
            }
            DbKind::Redis => return Err("Redis 不在这里管理用户".into()),
        })
    }

    pub fn drop_user_sql(self, name: &str, host: &str) -> Result<String, String> {
        if !valid_ident(name) {
            return Err("用户名里有不支持的字符,请到终端里处理".into());
        }
        Ok(match self {
            DbKind::Postgres => format!("DROP ROLE \"{name}\";\n"),
            DbKind::Mysql | DbKind::Mariadb => {
                if !valid_host(host) {
                    return Err("主机里有不支持的字符,请到终端里处理".into());
                }
                format!("DROP USER '{name}'@'{host}';\n")
            }
            DbKind::Redis => return Err("Redis 不在这里管理用户".into()),
        })
    }
}

// ---- 解析 ----

#[derive(Clone, Debug, PartialEq)]
pub struct DbRow {
    pub name: String,
    /// PostgreSQL 是所有者,MySQL/MariaDB 是字符集
    pub extra: String,
    /// PostgreSQL 的编码;MySQL/MariaDB 的字符集已在 `extra` 里,这里为空
    pub enc: String,
    /// 当前连接数
    pub conns: Option<u32>,
    pub size: Option<u64>,
    /// 排序规则
    pub collate: String,
    /// 表数量(仅 MySQL/MariaDB;PostgreSQL 要逐个连进每个库才能数,不取)
    pub tables: Option<u32>,
    /// 描述(仅 PostgreSQL 的库注释,换行已压成空格)
    pub desc: String,
    /// 系统库:只读
    pub system: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UserRow {
    pub name: String,
    /// MySQL/MariaDB 的主机;PostgreSQL 没有
    pub host: String,
    /// 权限说明(超级用户、可登录、可建库……)
    pub flags: String,
    /// 系统账号或当前登录的账号:不给删、不给改密码
    pub system: bool,
}

pub fn parse_dbs(kind: DbKind, out: &str) -> Vec<DbRow> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 3 || f[0].is_empty() {
                return None;
            }
            let system = match kind {
                DbKind::Postgres => matches!(f[0], "postgres" | "template0" | "template1"),
                _ => matches!(f[0], "mysql" | "sys" | "information_schema" | "performance_schema"),
            };
            let pg = kind == DbKind::Postgres;
            let at = |i: usize| f.get(i).map(|x| x.trim()).unwrap_or("");
            // PostgreSQL: 名称 所有者 编码 连接 大小 排序规则 描述;MySQL: 名称 字符集 连接 大小 排序规则 表数
            let row = if pg {
                DbRow { name: f[0].to_string(), extra: at(1).into(), enc: at(2).into(), conns: at(3).parse().ok(), size: at(4).parse().ok(), collate: at(5).into(), tables: None, desc: at(6).into(), system }
            } else {
                DbRow { name: f[0].to_string(), extra: at(1).into(), enc: String::new(), conns: at(2).parse().ok(), size: at(3).parse().ok(), collate: at(4).into(), tables: at(5).parse().ok(), desc: String::new(), system }
            };
            Some(row)
        })
        .collect()
}

pub fn parse_users(kind: DbKind, out: &str) -> Vec<UserRow> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            match kind {
                DbKind::Postgres if f.len() >= 5 && !f[0].is_empty() => {
                    let t = |s: &str| s.trim() == "t";
                    let mut flags = Vec::new();
                    if t(f[1]) {
                        flags.push("超级用户");
                    }
                    if !t(f[2]) {
                        flags.push("不能登录");
                    }
                    if t(f[3]) {
                        flags.push("可建库");
                    }
                    Some(UserRow { name: f[0].to_string(), host: String::new(), flags: flags.join("、"), system: f[0] == "postgres" || t(f[4]) })
                }
                DbKind::Mysql | DbKind::Mariadb if f.len() >= 2 && !f[0].is_empty() => {
                    let system = f[0] == "root" || f[0].starts_with("mysql.") || f[0].starts_with("mariadb.") || f[0] == "PUBLIC";
                    Some(UserRow { name: f[0].to_string(), host: f[1].to_string(), flags: String::new(), system })
                }
                _ => None,
            }
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RedisInfo {
    pub version: String,
    pub memory: String,
    pub clients: String,
    pub uptime_days: String,
    /// (库名如 db0, key 数)
    pub keys: Vec<(String, u64)>,
}

/// 解析 `redis-cli` 里 `INFO` 的输出(`key:value`,每行一项;`dbN:keys=3,expires=0,avg_ttl=0`)
pub fn parse_redis(out: &str) -> RedisInfo {
    let mut r = RedisInfo::default();
    for l in out.lines() {
        let Some((k, v)) = l.trim().split_once(':') else { continue };
        match k {
            "redis_version" => r.version = v.to_string(),
            "used_memory_human" => r.memory = v.to_string(),
            "connected_clients" => r.clients = v.to_string(),
            "uptime_in_days" => r.uptime_days = v.to_string(),
            _ if k.starts_with("db") && k[2..].chars().all(|c| c.is_ascii_digit()) && k.len() > 2 => {
                let n = v.split(',').find_map(|p| p.strip_prefix("keys=")).and_then(|n| n.parse().ok()).unwrap_or(0);
                r.keys.push((k.to_string(), n));
            }
            _ => {}
        }
    }
    r
}

/// 把容器里客户端的报错换成人看得懂的话
pub fn friendly_error(kind: DbKind, msg: &str) -> String {
    let m = msg.to_lowercase();
    if m.contains("is not running") || m.contains("not running") {
        "容器没有运行,请先启动容器".into()
    } else if m.contains("no such container") {
        "找不到这个容器,可能已经被删除".into()
    } else if m.contains("executable file not found") || m.contains("command not found") || m.contains("not found") && m.contains("psql") {
        format!("容器里没有 {} 的命令行客户端,没法管理", kind.label())
    } else if m.contains("password authentication failed") || m.contains("access denied") || m.contains("noauth") || m.contains("wrong password") || m.contains("authentication required") {
        "登录失败:容器的环境变量里没有可用的管理员密码(比如密码是用文件或其他方式配置的)".into()
    } else if m.contains("already exists") || m.contains("database exists") || m.contains("operation create user failed") {
        "已经存在同名的对象".into()
    } else if m.contains("is being accessed by other users") {
        "这个库正在被连接使用,先断开应用再删除".into()
    } else if m.contains("cannot be dropped because") || m.contains("role") && m.contains("cannot be dropped") {
        "这个用户还拥有库或权限,先转移或删除它们".into()
    } else {
        msg.to_string()
    }
}

/// 容量(字节)显示成 `12.3 MB`
pub fn fmt_size(b: u64) -> String {
    crate::docker::fmt_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ct(name: &str, image: &str, state: &str) -> Container {
        Container { id: name.into(), name: name.into(), image: image.into(), state: state.into(), status: String::new(), ports: vec![], project: None, service: None, panel: None, mounts: vec![] }
    }

    #[test]
    fn recognizes_common_images() {
        assert_eq!(kind_of_image("postgres:16"), Some(DbKind::Postgres));
        assert_eq!(kind_of_image("postgres"), Some(DbKind::Postgres));
        assert_eq!(kind_of_image("docker.io/library/mysql:8.0"), Some(DbKind::Mysql));
        assert_eq!(kind_of_image("registry.local:5000/team/mariadb:11@sha256:abc"), Some(DbKind::Mariadb));
        assert_eq!(kind_of_image("bitnami/postgresql:15"), Some(DbKind::Postgres));
        assert_eq!(kind_of_image("postgis/postgis:16-3.4"), Some(DbKind::Postgres));
        assert_eq!(kind_of_image("redis:7-alpine"), Some(DbKind::Redis));
        assert_eq!(kind_of_image("mysql/mysql-server:8.0"), Some(DbKind::Mysql));
    }

    #[test]
    fn ignores_related_but_not_databases() {
        assert_eq!(kind_of_image("prometheuscommunity/postgres-exporter:latest"), None);
        assert_eq!(kind_of_image("oliver006/redis_exporter"), None);
        assert_eq!(kind_of_image("redis-commander:latest"), None);
        assert_eq!(kind_of_image("nginx:1.25"), None);
        // 端口号里的冒号不是 tag
        assert_eq!(kind_of_image("localhost:5000/nginx"), None);
    }

    #[test]
    fn version_from_tag() {
        assert_eq!(split_image("postgres:16.2-alpine").1, "16.2-alpine");
        assert_eq!(split_image("localhost:5000/postgres").1, "latest");
    }

    #[test]
    fn running_first_then_by_name() {
        let v = detect(&[ct("b-pg", "postgres:16", "exited"), ct("z-redis", "redis:7", "running"), ct("a-web", "nginx", "running"), ct("a-my", "mysql:8", "running")]);
        let names: Vec<&str> = v.iter().map(|d| d.container.name.as_str()).collect();
        assert_eq!(names, ["a-my", "z-redis", "b-pg"]);
    }

    #[test]
    fn identifiers_and_hosts_are_strict() {
        for ok in ["app", "_x", "a1$b", "Order_Db"] {
            assert!(valid_ident(ok), "{ok}");
        }
        for bad in ["", "1a", "a-b", "a b", "a\"b", "a`b", "a'b", "a;drop", &"x".repeat(64)] {
            assert!(!valid_ident(bad), "{bad}");
        }
        assert!(valid_host("%") && valid_host("10.0.%") && valid_host("localhost") && valid_host("::1"));
        assert!(!valid_host("") && !valid_host("a'b") && !valid_host("a b") && !valid_host("%;"));
    }

    #[test]
    fn passwords_are_escaped_in_literals() {
        let q = DbKind::Postgres.create_user_sql("app", "", "p'a\\ss", "").unwrap();
        assert_eq!(q, "CREATE ROLE \"app\" LOGIN PASSWORD 'p''a\\ss';\n");
        let q = DbKind::Mysql.create_user_sql("app", "", "p'a\\ss", "shop").unwrap();
        assert_eq!(q, "CREATE USER 'app'@'%' IDENTIFIED BY 'p''a\\\\ss';\nGRANT ALL PRIVILEGES ON `shop`.* TO 'app'@'%';\n");
        // 引号永远成对出现:不可能逃出字面量
        let q = DbKind::Mariadb.alter_password_sql("app", "%", "x'; DROP DATABASE y; --").unwrap();
        assert!(q.contains("'x''; DROP DATABASE y; --'"));
        assert!(DbKind::Postgres.create_user_sql("app", "", "a\nb", "").is_err());
        assert!(DbKind::Postgres.create_user_sql("app", "", "", "").is_err());
    }

    #[test]
    fn sql_rejects_bad_names() {
        assert!(DbKind::Postgres.create_db_sql("a\"; drop", "").is_err());
        assert!(DbKind::Mysql.drop_db_sql("a`b").is_err());
        assert!(DbKind::Mysql.drop_user_sql("app", "%' OR '1").is_err());
        assert_eq!(DbKind::Postgres.create_db_sql("shop", "app").unwrap(), "CREATE DATABASE \"shop\" OWNER \"app\";\n");
        assert_eq!(DbKind::Mysql.drop_user_sql("app", "%").unwrap(), "DROP USER 'app'@'%';\n");
        assert!(DbKind::Redis.create_db_sql("x", "").is_err());
    }

    #[test]
    fn exec_args_have_no_user_input_and_no_password() {
        let a = DbKind::Postgres.exec_args("pg-main");
        assert_eq!(&a[..5], ["exec", "-i", "pg-main", "sh", "-c"]);
        assert!(a[5].contains("PGPASSWORD") && a[5].contains("$POSTGRES_PASSWORD") == false || a[5].contains("POSTGRES_PASSWORD"));
        // 脚本只引用环境变量名,不含任何具体的密码
        for k in [DbKind::Postgres, DbKind::Mysql, DbKind::Mariadb, DbKind::Redis] {
            assert!(k.client_script().starts_with(|c: char| c.is_alphabetic() || c == '[' || c == 'p' || c == 'u'));
        }
    }

    #[test]
    fn parses_postgres_output() {
        let dbs = parse_dbs(DbKind::Postgres, "postgres\tpostgres\tUTF8\t1\t7500000\ten_US.utf8\t\nshop\tapp\tUTF8\t3\t123456789\ten_US.utf8\t订单库\n");
        assert_eq!(dbs.len(), 2);
        assert!(dbs[0].system && !dbs[1].system);
        assert_eq!(dbs[1].extra, "app");
        assert_eq!(dbs[1].size, Some(123456789));
        assert_eq!(dbs[1].desc, "订单库");
        let us = parse_users(DbKind::Postgres, "app\tf\tt\tf\tf\npostgres\tt\tt\tt\tt\nreader\tf\tf\tf\tf\n");
        assert_eq!(us.len(), 3);
        assert!(!us[0].system && us[1].system);
        assert_eq!(us[1].flags, "超级用户、可建库");
        assert_eq!(us[2].flags, "不能登录");
    }

    #[test]
    fn parses_mysql_output() {
        let dbs = parse_dbs(DbKind::Mysql, "information_schema\tutf8mb3\t0\t0\tutf8mb3_general_ci\t60\nshop\tutf8mb4\t2\t16384\tutf8mb4_0900_ai_ci\t5\n");
        assert!(dbs[0].system && !dbs[1].system);
        assert_eq!((dbs[1].tables, dbs[1].collate.as_str(), dbs[1].size), (Some(5), "utf8mb4_0900_ai_ci", Some(16384)));
        let us = parse_users(DbKind::Mysql, "app\t%\nroot\tlocalhost\nmysql.sys\tlocalhost\n");
        assert!(!us[0].system && us[1].system && us[2].system);
        assert_eq!(us[0].host, "%");
        // 列数不对的行忽略
        assert!(parse_users(DbKind::Mysql, "oops\n").is_empty());
    }

    #[test]
    fn parses_redis_info() {
        let r = parse_redis("# Server\r\nredis_version:7.2.4\r\nuptime_in_days:3\r\nconnected_clients:2\r\nused_memory_human:1.02M\r\n# Keyspace\r\ndb0:keys=12,expires=1,avg_ttl=0\r\ndb3:keys=1,expires=0,avg_ttl=0\r\n");
        assert_eq!((r.version.as_str(), r.memory.as_str(), r.clients.as_str(), r.uptime_days.as_str()), ("7.2.4", "1.02M", "2", "3"));
        assert_eq!(r.keys, vec![("db0".to_string(), 12), ("db3".to_string(), 1)]);
    }

    #[test]
    fn friendly_errors() {
        assert!(friendly_error(DbKind::Postgres, "psql: error: connection to server failed: FATAL:  password authentication failed for user \"postgres\"").contains("登录失败"));
        assert!(friendly_error(DbKind::Mysql, "ERROR 1045 (28000): Access denied for user 'root'@'localhost'").contains("登录失败"));
        assert!(friendly_error(DbKind::Postgres, "ERROR:  database \"x\" already exists").contains("已经存在"));
        assert!(friendly_error(DbKind::Postgres, "Error response from daemon: container abc is not running").contains("没有运行"));
    }

    #[test]
    fn create_db_with_user() {
        let q = DbKind::Postgres.create_db_with_user_sql("app", "app_u", "pw-1", true, "订单库").unwrap();
        assert!(q.contains("CREATE ROLE \"app_u\" LOGIN PASSWORD 'pw-1'"));
        assert!(!q.contains("SUPERUSER"), "只给这个库的最高权限,不是集群超级用户");
        assert!(q.contains("CREATE DATABASE \"app\" OWNER \"app_u\""));
        assert!(q.contains("COMMENT ON DATABASE \"app\" IS '订单库'"));
        assert!(DbKind::Postgres.create_db_with_user_sql("app", "u", "pw", true, "第一行\n第二行").is_ok());
        let q = DbKind::Postgres.create_db_with_user_sql("app", "app_u", "pw-1", false, "").unwrap();
        assert!(q.contains("CREATE DATABASE \"app\";") && !q.contains("OWNER") && !q.contains("COMMENT"));
        assert!(q.contains("\\c \"app\"") && q.contains("GRANT CONNECT"));
        let q = DbKind::Mysql.create_db_with_user_sql("app", "u", "pw", true, "").unwrap();
        assert!(q.contains("`app`.*") && !q.contains("*.*") && q.contains("WITH GRANT OPTION"));
        let q = DbKind::Mysql.create_db_with_user_sql("app", "u", "pw", false, "").unwrap();
        assert!(q.contains("SELECT, INSERT") && !q.contains("ALL PRIVILEGES ON `app`"));
        assert!(DbKind::Postgres.create_db_with_user_sql("a;b", "u", "pw", false, "").is_err());
        assert!(DbKind::Postgres.create_db_with_user_sql("a", "u", "pw", false, "x\u{7}y").is_err());
        assert!(DbKind::Redis.create_db_with_user_sql("a", "u", "pw", false, "").is_err());
    }
}
