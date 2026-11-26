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

/// 取 Redis 密码的脚本片段(结果放在 `$p`):环境变量 → 启动参数 `--requirepass` → 配置文件里的 `requirepass`
/// (配置文件取启动参数里的 `*.conf`,否则试几个常见位置)。只在容器里执行,结果留在内存里
macro_rules! redis_pw_script {
    () => {
        concat!(
            r#"p="${REDIS_PASSWORD:-}"; "#,
            r#"[ -z "$p" ] && p=$(tr '\0' '\n' </proc/1/cmdline | sed -n '/^--requirepass$/{n;p;}'); "#,
            r#"if [ -z "$p" ]; then for f in $(tr '\0' '\n' </proc/1/cmdline | grep '\.conf$') /etc/redis/redis.conf /usr/local/etc/redis/redis.conf /data/redis.conf /redis.conf; do "#,
            r#"[ -r "$f" ] && p=$(sed -n 's/^[[:space:]]*requirepass[[:space:]][[:space:]]*//p' "$f" | head -n1 | sed 's/^"\(.*\)"$/\1/'); [ -n "$p" ] && break; done; fi; "#
        )
    };
}

/// 取 PostgreSQL 管理员账号密码的脚本片段(结果放在 `$u`、`$p`):环境变量(含 `*_FILE` 密钥文件)→ `~/.pgpass`
macro_rules! pg_pw_script {
    () => {
        concat!(
            r#"u="${POSTGRES_USER:-${POSTGRESQL_USERNAME:-postgres}}"; p="${POSTGRES_PASSWORD:-${POSTGRESQL_PASSWORD:-${POSTGRESQL_POSTGRES_PASSWORD:-}}}"; "#,
            r#"if [ -z "$p" ]; then for f in "$POSTGRES_PASSWORD_FILE" "$POSTGRESQL_PASSWORD_FILE" "$POSTGRESQL_POSTGRES_PASSWORD_FILE"; do [ -n "$f" ] && [ -r "$f" ] && p=$(head -n1 "$f") && [ -n "$p" ] && break; done; fi; "#,
            r#"if [ -z "$p" ]; then for f in "$HOME/.pgpass" /root/.pgpass /var/lib/postgresql/.pgpass; do [ -r "$f" ] && p=$(sed -n '1{s/^\([^:]*:\)\{4\}//;p;}' "$f") && [ -n "$p" ] && break; done; fi; "#
        )
    };
}

/// 取 MySQL/MariaDB root 密码的脚本片段(结果放在 `$p`):环境变量(含 `*_FILE` 密钥文件)→ `~/.my.cnf` 里的 `password=`
macro_rules! my_pw_script {
    () => {
        concat!(
            r#"p="${MYSQL_ROOT_PASSWORD:-${MARIADB_ROOT_PASSWORD:-}}"; "#,
            r#"if [ -z "$p" ]; then for f in "$MYSQL_ROOT_PASSWORD_FILE" "$MARIADB_ROOT_PASSWORD_FILE"; do [ -n "$f" ] && [ -r "$f" ] && p=$(head -n1 "$f") && [ -n "$p" ] && break; done; fi; "#,
            r#"if [ -z "$p" ]; then for f in "$HOME/.my.cnf" /root/.my.cnf; do [ -r "$f" ] && p=$(sed -n 's/^[[:space:]]*password[[:space:]]*=[[:space:]]*//p' "$f" | head -n1 | sed "s/^[\"']\(.*\)[\"']\$/\1/") && [ -n "$p" ] && break; done; fi; "#
        )
    };
}

/// 一个库上的权限档位
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// 只读:只能查询
    Read,
    /// 读写:查询、新增、修改、删除数据,不能改表结构
    Write,
    /// 全部权限
    All,
    /// 自定义:按 `Grant::perms` 里勾的项
    Custom,
}

/// 自定义权限的位:查询、新增、修改、删除、表结构(MySQL:建表/改表/删表/索引;PostgreSQL:在 public 里建表)
pub const P_SELECT: u8 = 1;
pub const P_INSERT: u8 = 2;
pub const P_UPDATE: u8 = 4;
pub const P_DELETE: u8 = 8;
pub const P_STRUCT: u8 = 16;
/// 读回来的授权带「可授权」(WITH GRANT OPTION):只用来在收回时一并去掉,不属于任何档位
pub const P_GRANTOPT: u8 = 32;

/// 给用户在某个库上授予的权限
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub db: String,
    pub level: Level,
    pub perms: u8,
}

impl Level {
    /// 这一档实际包含的权限位
    pub fn bits(self, custom: u8) -> u8 {
        match self {
            Level::Read => P_SELECT,
            Level::Write => P_SELECT | P_INSERT | P_UPDATE | P_DELETE,
            Level::All => P_SELECT | P_INSERT | P_UPDATE | P_DELETE | P_STRUCT,
            Level::Custom => custom & 31,
        }
    }
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
                concat!(pg_pw_script!(), r#"[ -n "$p" ] && export PGPASSWORD="$p"; exec psql -X -q -A -t -F "$(printf '\t')" -v ON_ERROR_STOP=1 -U "$u" -d postgres"#)
            }
            DbKind::Mysql | DbKind::Mariadb => {
                concat!(my_pw_script!(), r#"[ -n "$p" ] && export MYSQL_PWD="$p"; c=$(command -v mariadb || command -v mysql) || { echo "容器里没有 mysql 客户端" >&2; exit 1; }; exec "$c" -uroot -B -N"#)
            }
            DbKind::Redis => concat!(redis_pw_script!(), r#"[ -n "$p" ] && export REDISCLI_AUTH="$p"; exec redis-cli"#),
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

    /// 读连接信息(两行:用户名、密码)的脚本,取自容器自己的环境变量。只在「连接信息」弹窗打开时执行,
    /// 结果只放在内存里给界面显示,不写日志、不经过 agent;Redis 没有用户名(第一行为空)
    pub fn conn_args(self, container: &str) -> Vec<String> {
        let script = match self {
            DbKind::Postgres => concat!(pg_pw_script!(), r#"printf '%s\n%s' "$u" "$p""#),
            DbKind::Mysql | DbKind::Mariadb => concat!(my_pw_script!(), r#"printf 'root\n%s' "$p""#),
            DbKind::Redis => concat!(redis_pw_script!(), r#"printf '\n%s' "$p""#),
        };
        ["exec", container, "sh", "-c", script].map(String::from).to_vec()
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

    /// 新建用户并按列表授权(每个库一档权限;只对现有的库授权,以后新建的库不会自动授权)
    pub fn create_user_with_grants_sql(self, name: &str, host: &str, password: &str, grants: &[Grant]) -> Result<String, String> {
        let mut q = self.create_user_sql(name, host, password, "")?;
        q += &self.grants_sql(name, host, grants)?;
        Ok(q)
    }

    pub fn grants_sql(self, user: &str, host: &str, grants: &[Grant]) -> Result<String, String> {
        let mut q = String::new();
        for g in grants {
            if !valid_ident(&g.db) {
                return Err("库名里有不支持的字符,请到终端里处理".into());
            }
            let bits = g.level.bits(g.perms);
            if bits == 0 {
                return Err(format!("库 {} 还没有勾选任何权限", g.db));
            }
            let db = &g.db;
            match self {
                DbKind::Mysql | DbKind::Mariadb => {
                    let host = if host.is_empty() { "%" } else { host };
                    if !valid_host(host) {
                        return Err("主机只能用字母、数字和 . % : - _".into());
                    }
                    let privs = if g.level == Level::All {
                        "ALL PRIVILEGES".to_string()
                    } else {
                        let mut v: Vec<&str> = Vec::new();
                        for (b, n) in [(P_SELECT, "SELECT"), (P_INSERT, "INSERT"), (P_UPDATE, "UPDATE"), (P_DELETE, "DELETE")] {
                            if bits & b != 0 {
                                v.push(n);
                            }
                        }
                        if g.level == Level::Read {
                            v.push("SHOW VIEW");
                        }
                        if bits & P_STRUCT != 0 {
                            v.extend(["CREATE", "ALTER", "DROP", "INDEX"]);
                        }
                        v.join(", ")
                    };
                    q += &format!("GRANT {privs} ON `{db}`.* TO '{user}'@'{host}';\n");
                }
                DbKind::Postgres => {
                    if g.level == Level::All {
                        q += &format!("GRANT ALL PRIVILEGES ON DATABASE \"{db}\" TO \"{user}\";\n");
                    } else {
                        q += &format!("GRANT CONNECT ON DATABASE \"{db}\" TO \"{user}\";\n");
                    }
                    // 表和序列在库里面授权:连进这个库再授(psql 的 \c 沿用 PGPASSWORD)
                    q += &format!("\\c \"{db}\"\n");
                    let schema = if bits & P_STRUCT != 0 { "USAGE, CREATE" } else { "USAGE" };
                    q += &format!("GRANT {schema} ON SCHEMA public TO \"{user}\";\n");
                    let tbl: Vec<&str> = [(P_SELECT, "SELECT"), (P_INSERT, "INSERT"), (P_UPDATE, "UPDATE"), (P_DELETE, "DELETE")].iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| *n).collect();
                    if !tbl.is_empty() {
                        let t = if g.level == Level::All { "ALL PRIVILEGES".to_string() } else { tbl.join(", ") };
                        let seq = if g.level == Level::All { "ALL PRIVILEGES" } else if bits & (P_INSERT | P_UPDATE) != 0 { "USAGE, SELECT" } else { "SELECT" };
                        q += &format!("GRANT {t} ON ALL TABLES IN SCHEMA public TO \"{user}\";\n");
                        q += &format!("GRANT {seq} ON ALL SEQUENCES IN SCHEMA public TO \"{user}\";\n");
                        q += &format!("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT {t} ON TABLES TO \"{user}\";\n");
                        q += &format!("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT {seq} ON SEQUENCES TO \"{user}\";\n");
                    }
                }
                DbKind::Redis => return Err("Redis 不在这里管理用户".into()),
            }
        }
        Ok(q)
    }

    /// 读一个用户现有的按库授权(PostgreSQL 要逐库连进去查,所以要传库名列表;MySQL 查 `mysql.db`)。输出用 `parse_grants` 解析
    pub fn read_grants_sql(self, user: &str, host: &str, dbs: &[String]) -> Result<String, String> {
        if !valid_ident(user) {
            return Err("用户名里有不支持的字符,请到终端里处理".into());
        }
        match self {
            DbKind::Mysql | DbKind::Mariadb => {
                if !valid_host(host) {
                    return Err("主机里有不支持的字符,请到终端里处理".into());
                }
                Ok(format!("SELECT Db, Select_priv, Insert_priv, Update_priv, Delete_priv, Create_priv, Drop_priv, Alter_priv, Index_priv, Show_view_priv, References_priv, Create_tmp_table_priv, Lock_tables_priv, Create_view_priv, Create_routine_priv, Alter_routine_priv, Execute_priv, Event_priv, Trigger_priv, Grant_priv FROM mysql.db WHERE User = '{user}' AND Host = '{host}' ORDER BY Db;\n"))
            }
            DbKind::Postgres => {
                let mut q = String::new();
                for d in dbs.iter().filter(|d| valid_ident(d)) {
                    q += &format!("\\c \"{d}\"\n");
                    // 字段:库名、是否有显式授权、库上 CREATE、public 上 CREATE、表的 SELECT/INSERT/UPDATE/DELETE(有表看表,没表看默认权限)
                    let role = format!("(SELECT oid FROM pg_roles WHERE rolname = '{user}')");
                    let tbl = |p: &str| format!("COALESCE((SELECT bool_and(has_table_privilege('{user}', c.oid, '{p}')) FROM pg_class c WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r','p')), EXISTS (SELECT 1 FROM pg_default_acl d, aclexplode(d.defaclacl) a WHERE a.grantee = {role} AND d.defaclobjtype = 'r' AND d.defaclnamespace = 'public'::regnamespace AND a.privilege_type = '{p}'))");
                    q += &format!(
                        "SELECT current_database(), EXISTS (SELECT 1 FROM pg_database x, aclexplode(x.datacl) a WHERE x.datname = current_database() AND a.grantee = {role}) OR EXISTS (SELECT 1 FROM pg_database x WHERE x.datname = current_database() AND x.datdba = {role}), has_database_privilege('{user}', current_database(), 'CREATE'), EXISTS (SELECT 1 FROM pg_namespace n, aclexplode(n.nspacl) a WHERE n.nspname = 'public' AND a.grantee = {role} AND a.privilege_type = 'CREATE') OR EXISTS (SELECT 1 FROM pg_database x WHERE x.datname = current_database() AND x.datdba = {role}), {}, {}, {}, {};\n",
                        tbl("SELECT"),
                        tbl("INSERT"),
                        tbl("UPDATE"),
                        tbl("DELETE")
                    );
                }
                Ok(q)
            }
            DbKind::Redis => Err("Redis 不在这里管理用户".into()),
        }
    }

    /// 解析 `read_grants_sql` 的输出成按库授权;MySQL 带通配符的库名(如 `app\_%`)不是单个库,跳过
    pub fn parse_grants(self, out: &str) -> Vec<Grant> {
        let mut v = Vec::new();
        for l in out.lines() {
            let f: Vec<&str> = l.trim_end_matches('\r').split('\t').collect();
            match self {
                DbKind::Postgres => {
                    if f.len() < 8 || f[1] != "t" {
                        continue;
                    }
                    let b = |i: usize| f[i] == "t";
                    let (dbc, sch) = (b(2), b(3));
                    let bits = u8::from(b(4)) * P_SELECT | u8::from(b(5)) * P_INSERT | u8::from(b(6)) * P_UPDATE | u8::from(b(7)) * P_DELETE | u8::from(sch) * P_STRUCT;
                    let level = if dbc && sch && bits == 31 {
                        Level::All
                    } else if bits == P_SELECT {
                        Level::Read
                    } else if bits == Level::Write.bits(0) {
                        Level::Write
                    } else {
                        Level::Custom
                    };
                    v.push(Grant { db: f[0].to_string(), level, perms: bits });
                }
                DbKind::Mysql | DbKind::Mariadb => {
                    if f.len() < 20 || f[0].contains(['%', '*']) {
                        continue;
                    }
                    let y = |i: usize| f[i] == "Y";
                    let bits = u8::from(y(1)) * P_SELECT | u8::from(y(2)) * P_INSERT | u8::from(y(3)) * P_UPDATE | u8::from(y(4)) * P_DELETE | u8::from(y(5) || y(6) || y(7) || y(8)) * P_STRUCT;
                    let level = if (1..19).all(y) {
                        Level::All
                    } else if bits == P_SELECT && y(9) && (10..19).all(|i| !y(i)) {
                        Level::Read
                    } else if bits == Level::Write.bits(0) && (9..19).all(|i| !y(i)) && !(y(5) || y(6) || y(7) || y(8)) {
                        Level::Write
                    } else {
                        Level::Custom
                    };
                    v.push(Grant { db: f[0].replace("\\_", "_"), level, perms: bits | if y(19) { P_GRANTOPT } else { 0 } });
                }
                DbKind::Redis => {}
            }
        }
        v
    }

    /// 把用户的按库授权从 `old` 改成 `new`:没变的不动,变了或移除的先收回再按新的授;新加的直接授
    pub fn update_grants_sql(self, user: &str, host: &str, old: &[Grant], new: &[Grant]) -> Result<String, String> {
        let same = |a: &Grant, b: &Grant| a.db == b.db && a.level == b.level && a.level.bits(a.perms) == b.level.bits(b.perms);
        let mut q = String::new();
        for g in old {
            if new.iter().any(|n| same(n, g)) {
                continue;
            }
            if !valid_ident(&g.db) {
                return Err("库名里有不支持的字符,请到终端里处理".into());
            }
            let db = &g.db;
            match self {
                DbKind::Mysql | DbKind::Mariadb => {
                    let host = if host.is_empty() { "%" } else { host };
                    if !valid_host(host) {
                        return Err("主机里有不支持的字符,请到终端里处理".into());
                    }
                    q += &format!("REVOKE ALL PRIVILEGES ON `{db}`.* FROM '{user}'@'{host}';\n");
                    if g.perms & P_GRANTOPT != 0 {
                        q += &format!("REVOKE GRANT OPTION ON `{db}`.* FROM '{user}'@'{host}';\n");
                    }
                }
                DbKind::Postgres => {
                    q += &format!("REVOKE ALL ON DATABASE \"{db}\" FROM \"{user}\";\n\\c \"{db}\"\n");
                    q += &format!("REVOKE ALL ON ALL TABLES IN SCHEMA public FROM \"{user}\";\nREVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM \"{user}\";\nREVOKE ALL ON SCHEMA public FROM \"{user}\";\n");
                    q += &format!("ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON TABLES FROM \"{user}\";\nALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON SEQUENCES FROM \"{user}\";\n");
                }
                DbKind::Redis => return Err("Redis 不在这里管理用户".into()),
            }
        }
        let add: Vec<Grant> = new.iter().filter(|n| !old.iter().any(|o| same(n, o))).cloned().collect();
        q += &self.grants_sql(user, host, &add)?;
        Ok(q)
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
    pub role: String,
    pub mode: String,
    pub max_memory: String,
    pub peak_memory: String,
    pub ops: String,
    pub commands: String,
    pub hits: u64,
    pub misses: u64,
    pub expired: String,
    pub evicted: String,
    /// (库名如 db0, key 数)
    pub keys: Vec<(String, u64)>,
}

/// `INFO` 没拿到数据(要密码 / 密码不对)时给人看得懂的话
pub fn redis_check(out: &str) -> Result<(), String> {
    if out.contains("redis_version:") {
        return Ok(());
    }
    let l = out.trim();
    if l.contains("NOAUTH") || l.contains("WRONGPASS") || l.contains("invalid password") {
        Err("Redis 需要密码,但没能从容器环境变量或启动参数里取到(可能是从别处加载的)".into())
    } else if l.is_empty() {
        Err("没有读到 Redis 信息".into())
    } else {
        Err(l.lines().next().unwrap_or(l).to_string())
    }
}

/// 解析 `redis-cli` 里 `INFO` 的输出(`key:value`,每行一项;`dbN:keys=3,expires=0,avg_ttl=0`)
pub fn parse_redis(out: &str) -> RedisInfo {
    // (调用方先用 `redis_check` 判断是不是正常输出)
    let mut r = RedisInfo::default();
    for l in out.lines() {
        let Some((k, v)) = l.trim().split_once(':') else { continue };
        match k {
            "redis_version" => r.version = v.to_string(),
            "used_memory_human" => r.memory = v.to_string(),
            "connected_clients" => r.clients = v.to_string(),
            "uptime_in_days" => r.uptime_days = v.to_string(),
            "role" => r.role = v.to_string(),
            "redis_mode" => r.mode = v.to_string(),
            "maxmemory_human" => r.max_memory = v.to_string(),
            "used_memory_peak_human" => r.peak_memory = v.to_string(),
            "instantaneous_ops_per_sec" => r.ops = v.to_string(),
            "total_commands_processed" => r.commands = v.to_string(),
            "keyspace_hits" => r.hits = v.parse().unwrap_or(0),
            "keyspace_misses" => r.misses = v.parse().unwrap_or(0),
            "expired_keys" => r.expired = v.to_string(),
            "evicted_keys" => r.evicted = v.to_string(),
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
/// 数据库进程还在启动/初始化(刚安装完、刚重启):连不上不算失败,过几秒会好
/// URI 里的用户名/密码要转义:只保留字母数字和 `-._~`,其余按 UTF-8 字节写成 `%XX`
fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// 连接串:`postgresql://用户:密码@主机:端口`、`mysql://…`、`redis://:密码@主机:端口`。`pw` 为空就不带密码;
/// 主机是 IPv6 地址时加方括号。`mask` 为 true 时密码位置写成 `••••`(只用于界面显示,复制用 false)
pub fn conn_uri(kind: DbKind, user: &str, pw: &str, host: &str, port: &str, mask: bool) -> String {
    let scheme = match kind {
        DbKind::Postgres => "postgresql",
        DbKind::Mysql | DbKind::Mariadb => "mysql",
        DbKind::Redis => "redis",
    };
    let pwd = if pw.is_empty() { String::new() } else if mask { "••••".to_string() } else { pct(pw) };
    let auth = match (user.is_empty(), pwd.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!(":{pwd}@"),
        (false, true) => format!("{}@", pct(user)),
        (false, false) => format!("{}:{pwd}@", pct(user)),
    };
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    format!("{scheme}://{auth}{host}:{port}")
}

/// 服务器上列出本机 IPv4 地址的脚本(一行一个,跳过 docker/虚拟网卡)
pub const ADDR_SCRIPT: &str = r#"ip -o -4 addr show scope global 2>/dev/null | awk '$2 !~ /^(docker|br-|veth|cni|flannel|virbr|lo)/ {print $4}' | cut -d/ -f1"#;

/// 私有地址(10/8、172.16/12、192.168/16)和运营商级 NAT(100.64/10)
pub fn is_private_v4(ip: &str) -> bool {
    let p: Vec<u8> = ip.split('.').filter_map(|x| x.parse().ok()).collect();
    p.len() == 4 && (p[0] == 10 || (p[0] == 172 && (16..=31).contains(&p[1])) || (p[0] == 192 && p[1] == 168) || (p[0] == 100 && (64..=127).contains(&p[1])))
}

/// 把地址列表分成 (内网, 公网);非 IPv4 的行忽略。`node_host` 是节点的连接地址:公网 IP 或域名也算公网地址
pub fn classify_addrs(out: &str, node_host: &str) -> (Vec<String>, Vec<String>) {
    let (mut private, mut public) = (Vec::new(), Vec::new());
    let mut add = |ip: &str| {
        let ip = ip.trim();
        if ip.split('.').count() != 4 || ip.split('.').any(|x| x.parse::<u8>().is_err()) || ip.starts_with("127.") {
            return;
        }
        let list = if is_private_v4(ip) { &mut private } else { &mut public };
        if !list.iter().any(|x| x == ip) {
            list.push(ip.to_string());
        }
    };
    out.lines().for_each(&mut add);
    // 节点地址本身:是公网 IP 就算(云服务器的弹性公网 IP 不在网卡上);是域名也算公网
    let h = node_host.trim();
    let is_ip = h.split('.').count() == 4 && h.split('.').all(|x| x.parse::<u8>().is_ok());
    if is_ip {
        add(h);
    } else if !h.is_empty() && !h.contains(':') && !public.iter().any(|x| x == h) {
        public.push(h.to_string());
    }
    (private, public)
}

pub fn is_starting(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["can't connect to local", "connection refused", "starting up", "is the server running", "lost connection to mysql server", "closed the connection unexpectedly", "could not connect to server", "loading the dataset"].iter().any(|k| m.contains(k))
}

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

// ---- 备份与恢复 -----------------------------------------------------------------
//
// 备份文件放服务器 `/opt/otter/databases/backups/<容器名>/`(见 srvdata;登录用户没权限建这个目录时退回 `~/otter-backups/<容器名>/`),文件名 `<库名>_<YYYYmmdd-HHMMSS>[-pre].(dump|sql.gz)`。
// 脚本在服务器上跑:`docker exec` 容器里的 pg_dump / mysqldump,账号密码在容器里读容器自己的环境变量(沿用 `client_script`),
// 不经过 Otter。长任务脚本自己脱离运行(忽略 HUP),先写 `.part`,成功才改名;失败留 `.fail`(里面是原因)。

/// 备份目录里的文件名只允许这些字符(删除、恢复、下载前校验,防止 `../` 之类)
pub fn valid_backup_file(s: &str) -> bool {
    !s.is_empty() && s.len() <= 120 && !s.starts_with('.') && !s.contains("..") && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.' | '-'))
}

/// 容器名(docker 的命名规则)
pub fn valid_container(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// 单引号包起来,里面的单引号转义
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 一份备份(列表里的一行)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupFile {
    pub name: String,
    pub db: String,
    pub size: u64,
    /// 修改时间(Unix 秒)
    pub mtime: u64,
    /// 恢复前自动做的那份
    pub pre: bool,
}

/// 备份列表脚本的解析结果(已按库过滤)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Backups {
    /// 备份所在目录的绝对路径(下载要用)
    pub dir: String,
    /// 旧位置 `~/otter-backups/<容器>/` 里还有备份:(个数, 占用 KiB),等用户确认迁移
    pub legacy: Option<(usize, u64)>,
    pub files: Vec<BackupFile>,
    /// 正在写的 `.part`:(文件名, 已写大小)
    pub running: Vec<(String, u64)>,
    /// 失败的备份:(文件名, 原因)
    pub failed: Vec<(String, String)>,
    /// 这个库的恢复状态:`running` / `ok` / `fail <原因>`
    pub restore: Option<String>,
}

impl DbKind {
    /// 备份文件扩展名;Redis 不支持
    pub fn backup_ext(self) -> Option<&'static str> {
        match self {
            DbKind::Postgres => Some("dump"),
            DbKind::Mysql | DbKind::Mariadb => Some("sql.gz"),
            DbKind::Redis => None,
        }
    }

    /// 容器里导出一个库(库名从环境变量 `OTG_DB` 取),数据写到 stdout
    fn dump_script(self) -> &'static str {
        match self {
            DbKind::Postgres => concat!(pg_pw_script!(), r#"[ -n "$p" ] && export PGPASSWORD="$p"; exec pg_dump -Fc -U "$u" -d "$OTG_DB""#),
            DbKind::Mysql | DbKind::Mariadb => concat!(
                my_pw_script!(),
                r#"[ -n "$p" ] && export MYSQL_PWD="$p"; c=$(command -v mariadb-dump || command -v mysqldump) || { echo "容器里没有 mysqldump" >&2; exit 1; }; exec "$c" -uroot --single-transaction --routines --triggers --events "$OTG_DB""#
            ),
            DbKind::Redis => "exit 1",
        }
    }

    /// 容器里导入一个库(从 stdin 读备份内容)。PG 用单个事务,失败整体回滚
    fn restore_inner(self) -> &'static str {
        match self {
            DbKind::Postgres => concat!(
                pg_pw_script!(),
                r#"[ -n "$p" ] && export PGPASSWORD="$p"; exec pg_restore --clean --if-exists --single-transaction --exit-on-error -U "$u" -d "$OTG_DB""#
            ),
            DbKind::Mysql | DbKind::Mariadb => {
                concat!(my_pw_script!(), r#"[ -n "$p" ] && export MYSQL_PWD="$p"; c=$(command -v mariadb || command -v mysql) || { echo "容器里没有 mysql 客户端" >&2; exit 1; }; exec "$c" -uroot "$OTG_DB""#)
            }
            DbKind::Redis => "exit 1",
        }
    }

    /// 脚本开头:算出备份目录 `$D`(登录用户主目录下),必要时建好,磁盘够用
    fn backup_prelude(ctr: &str) -> String {
        format!(
            r#"command -v docker >/dev/null 2>&1 || {{ echo "找不到 docker" >&2; exit 1; }}; U="${{SUDO_USER:-$(id -un)}}"; H=$(getent passwd "$U" | cut -d: -f6); [ -n "$H" ] || H="$HOME"; LG="$H/otter-backups/{ctr}"; D="{bk}/{ctr}"; mkdir -p "$D" 2>/dev/null && [ -w "$D" ] || D="$LG"; mkdir -p "$D" || exit 1; chmod 700 "$D"; chown "$U" "$D" 2>/dev/null; [ "$D" = "$LG" ] && chown "$U" "$H/otter-backups" 2>/dev/null; umask 077; "#,
            bk = format!("{}/backups", crate::srvdata::DB)
        )
    }

    /// `dump_to <文件名>`:导出 `OTG_DB` 到 `$D` 下的文件;成功改名,失败留 `<文件名>.fail`(原因)
    fn dump_fn(self, ctr: &str, db: &str) -> String {
        let s = sq(self.dump_script());
        let run = format!(r#"docker exec -e OTG_DB={db} {ctr} sh -c {s}"#);
        if self == DbKind::Postgres {
            format!(
                r#"dump_to() {{ {run} >"$1.part" 2>"$1.err" </dev/null; r=$?; if [ "$r" = 0 ]; then mv "$1.part" "$1"; rm -f "$1.err"; else rm -f "$1.part"; mv "$1.err" "$1.fail"; fi; chown "$U" "$1" "$1.fail" 2>/dev/null; return "$r"; }}; "#
            )
        } else {
            format!(
                r#"dump_to() {{ {{ {run} 2>"$1.err" </dev/null; echo $? >"$1.rc"; }} | gzip >"$1.part"; r=$(cat "$1.rc" 2>/dev/null); rm -f "$1.rc"; if [ "$r" = 0 ]; then mv "$1.part" "$1"; rm -f "$1.err"; else r=1; rm -f "$1.part"; mv "$1.err" "$1.fail"; fi; chown "$U" "$1" "$1.fail" 2>/dev/null; return "$r"; }}; "#
            )
        }
    }

    /// 立即备份一个库:脚本检查磁盘、启动后台导出就返回(输出 `started`)
    pub fn backup_script(self, ctr: &str, db: &str) -> Result<String, String> {
        let ext = self.backup_ext().ok_or("Redis 不支持备份")?;
        if !valid_container(ctr) || !valid_ident(db) {
            return Err("容器名或库名里有不支持的字符,请到终端里处理".into());
        }
        Ok(format!(
            r#"{}a=$(df -Pk "$D" | awk 'NR==2{{print $4}}'); [ "${{a:-0}}" -ge 102400 ] || {{ echo "服务器磁盘剩余空间不足 100MB,无法备份" >&2; exit 1; }}; {}F="{db}_$(date +%Y%m%d-%H%M%S).{ext}"; ( sh -c 'echo $PPID' >"$D/.pid-$F"; trap '' HUP; cd "$D" || exit 1; dump_to "$F"; rm -f "$D/.pid-$F" ) </dev/null >/dev/null 2>&1 & echo started
"#,
            Self::backup_prelude(ctr),
            self.dump_fn(ctr, db)
        ))
    }

    /// 恢复:`pre` 为真时先做一份「恢复前」备份(失败就取消),再导入;默认由用户决定,不替用户做主。进度写在 `$D/.restore-<库>`
    pub fn restore_script(self, ctr: &str, db: &str, file: &str, pre: bool) -> Result<String, String> {
        let ext = self.backup_ext().ok_or("Redis 不支持恢复")?;
        if !valid_container(ctr) || !valid_ident(db) || !valid_backup_file(file) {
            return Err("容器名、库名或文件名里有不支持的字符".into());
        }
        if !file.starts_with(&format!("{db}_")) || !file.ends_with(&format!(".{ext}")) {
            return Err("这份备份不属于这个库".into());
        }
        let s = sq(self.restore_inner());
        let feed = if self == DbKind::Postgres { format!(r#"docker exec -i -e OTG_DB={db} {ctr} sh -c {s} <"$F" 2>"$ST.err""#) } else { format!(r#"gunzip -c "$F" | docker exec -i -e OTG_DB={db} {ctr} sh -c {s} 2>"$ST.err""#) };
        let (space, dump) = if pre {
            (
                r#"a=$(df -Pk "$D" | awk 'NR==2{print $4}'); [ "${a:-0}" -ge 102400 ] || { echo "服务器磁盘剩余空间不足 100MB,无法先做恢复前备份" >&2; exit 1; }; "#.to_string(),
                format!(r#"P="{db}_$(date +%Y%m%d-%H%M%S)-pre.{ext}"; if ! dump_to "$P"; then st "fail 恢复前备份失败,已取消恢复"; exit 1; fi; "#),
            )
        } else {
            (String::new(), String::new())
        };
        Ok(format!(
            r#"{}{space}{}F={file_q}; [ -f "$D/$F" ] || {{ echo "备份文件不存在" >&2; exit 1; }}; ST="$D/.restore-{db}"; case "$F" in *.sql.gz) gzip -t "$D/$F" || {{ echo "备份文件已损坏(gzip 校验失败)" >&2; exit 1; }};; esac; if [ -f "$ST" ] && [ "$(head -n1 "$ST")" = running ]; then echo "这个库正在恢复中" >&2; exit 1; fi; st() {{ printf '%s\n' "$1" >"$ST"; chown "$U" "$ST" 2>/dev/null; }}; st running; ( trap '' HUP; cd "$D" || exit 1; {dump}F="$D/$F"; if {feed}; then st ok; else m=$(tail -n1 "$ST.err" 2>/dev/null | tr '\t' ' '); st "fail ${{m:-恢复失败}}"; fi; rm -f "$ST.err" ) </dev/null >/dev/null 2>&1 & echo started
"#,
            Self::backup_prelude(ctr),
            self.dump_fn(ctr, db),
            file_q = sq(file),
        ))
    }
}

/// 备份列表脚本(登录用户身份即可):`H` 主目录、`F` 完成的备份、`P` 进行中、`E` 失败、`R` 恢复状态
pub fn backup_list_script(ctr: &str) -> Result<String, String> {
    if !valid_container(ctr) {
        return Err("容器名里有不支持的字符".into());
    }
    Ok(format!(
        r#"LG="$HOME/otter-backups/{ctr}"; D="{bk}/{ctr}"; [ -d "$D" ] || D="$LG"; printf 'H\t%s\n' "$D"; if [ "$D" != "$LG" ] && [ -d "$LG" ]; then n=$(ls -1p "$LG" 2>/dev/null | grep -v '/$' | grep -vE '\.(part|err|rc|fail)$' | wc -l); [ "$n" -gt 0 ] && printf 'L\t%s\t%s\n' "$n" "$(du -sk "$LG" | cut -f1)"; fi; cd "$D" 2>/dev/null || exit 0; for f in *; do [ -f "$f" ] || continue; case "$f" in *.part) printf 'P\t%s\t%s\n' "$f" "$(stat -c %s "$f")";; *.fail) printf 'E\t%s\t%s\n' "$f" "$(tail -n1 "$f" | tr '\t' ' ')";; *.err|*.rc) ;; *) printf 'F\t%s\t%s\t%s\n' "$f" "$(stat -c %s "$f")" "$(stat -c %Y "$f")";; esac; done; for f in .restore-*; do [ -f "$f" ] && printf 'R\t%s\t%s\n' "${{f#.restore-}}" "$(head -n1 "$f" | tr '\t' ' ')"; done; true
"#,
        bk = format!("{}/backups", crate::srvdata::DB)
    ))
}

/// 把旧位置 `~/otter-backups/<容器名>/` 的备份迁到 `/opt/otter/databases/backups/<容器名>/`(同盘瞬间完成,跨盘 mv 会复制再删);已存在同名文件的跳过。输出 `migrated <个数>`
pub fn backup_migrate_script(ctr: &str) -> Result<String, String> {
    if !valid_container(ctr) {
        return Err("容器名里有不支持的字符".into());
    }
    Ok(format!(
        r#"U="${{SUDO_USER:-$(id -un)}}"; H=$(getent passwd "$U" | cut -d: -f6); [ -n "$H" ] || H="$HOME"; LG="$H/otter-backups/{ctr}"; D="{bk}/{ctr}"; mkdir -p "$D" 2>/dev/null && [ -w "$D" ] || {{ echo "当前账号不能写 {bk},请用有 root 权限的账号迁移" >&2; exit 1; }}; chmod 700 "$D"; chown "$U" "$D" 2>/dev/null; n=0; for f in "$LG"/*; do [ -f "$f" ] || continue; b=$(basename "$f"); [ -e "$D/$b" ] && continue; mv "$f" "$D/$b" || {{ echo "移动 $b 失败" >&2; exit 1; }}; chown "$U" "$D/$b" 2>/dev/null; n=$((n+1)); done; rmdir "$LG" 2>/dev/null; echo "migrated $n"
"#,
        bk = format!("{}/backups", crate::srvdata::DB)
    ))
}

/// 中止一个进行中的备份(`.part` 文件):杀掉后台进程树、清掉半截文件。进程在服务器上脱离运行,靠备份时写的 `.pid-<文件名>` 找到它
pub fn backup_cancel_script(ctr: &str, part: &str) -> Result<String, String> {
    if !valid_container(ctr) || !valid_backup_file(part) || !part.ends_with(".part") {
        return Err("文件名里有不支持的字符".into());
    }
    let base = part.trim_end_matches(".part");
    let db = base.split('.').next().and_then(|st| st.rsplit_once('_')).map(|(d, _)| d).filter(|d| valid_ident(d)).ok_or("文件名里没有库名")?;
    // 容器里的导出进程是 `docker exec -e OTG_DB=<库>` 起的,靠环境变量认出来一并结束(只杀导出程序)
    let inner = sq(&format!(r#"for d in /proc/[0-9]*; do tr '\0' '\n' <"$d/environ" 2>/dev/null | grep -qx 'OTG_DB={db}' || continue; case "$(cat "$d/comm" 2>/dev/null)" in pg_dump|mysqldump|mariadb-dump) kill "${{d#/proc/}}";; esac; done"#));
    Ok(format!(
        r#"U="${{SUDO_USER:-$(id -un)}}"; H=$(getent passwd "$U" | cut -d: -f6); [ -n "$H" ] || H="$HOME"; D="{bk}/{ctr}"; [ -d "$D" ] || D="$H/otter-backups/{ctr}"; B='{base}'; P="$D/.pid-$B"; kids() {{ ps -eo pid=,ppid= | awk -v p="$1" '$2==p{{print $1}}'; }}; kt() {{ for c in $(kids "$1"); do kt "$c"; done; kill "$1" 2>/dev/null; }}; if [ -f "$P" ]; then pid=$(cat "$P"); case "$pid" in ''|*[!0-9]*) ;; *) n=$(ps -o comm= -p "$pid" 2>/dev/null); case "$n" in sh|dash|bash|ash) ks=$(kids "$pid"); kill "$pid" 2>/dev/null; for k in $ks; do kt "$k"; done;; esac;; esac; fi; docker exec {ctr} sh -c {inner} >/dev/null 2>&1; sleep 1; rm -f "$D/$B.part" "$D/$B.err" "$D/$B.rc" "$P"; echo cancelled
"#,
        bk = format!("{}/backups", crate::srvdata::DB)
    ))
}

/// 删除一份备份(或一条失败记录)
pub fn backup_delete_script(ctr: &str, file: &str) -> Result<String, String> {
    if !valid_container(ctr) || !valid_backup_file(file) || file.ends_with(".part") {
        return Err("文件名里有不支持的字符".into());
    }
    Ok(format!("D=\"{bk}/{ctr}\"; [ -d \"$D\" ] || D=\"$HOME/otter-backups/{ctr}\"; rm -f \"$D/\"{}\n", sq(file), bk = format!("{}/backups", crate::srvdata::DB)))
}

/// 服务器上的备份文件绝对路径
pub fn backup_remote_path(dir: &str, file: &str) -> Option<String> {
    (valid_backup_file(file) && dir.starts_with('/') && !dir.contains(['\'', '\n'])).then(|| format!("{}/{file}", dir.trim_end_matches('/')))
}

/// 解析 `backup_list_script` 的输出,只留属于 `db` 的
pub fn parse_backups(out: &str, db: &str) -> Backups {
    let mut b = Backups::default();
    let mine = |name: &str| name.strip_prefix(db).is_some_and(|r| r.starts_with('_'));
    // 文件名 = <库名>_<时间戳>[-pre].<扩展名>;库名里可以有下划线,所以从右边切
    let owner = |name: &str| -> Option<(String, bool)> {
        let stem = name.split('.').next()?;
        let (d, ts) = stem.rsplit_once('_')?;
        Some((d.to_string(), ts.ends_with("-pre")))
    };
    for l in out.lines() {
        let f: Vec<&str> = l.trim_end_matches('\r').split('\t').collect();
        match f.as_slice() {
            ["H", h] => b.dir = h.to_string(),
            ["L", n, kb] => b.legacy = Some((n.parse().unwrap_or(0), kb.parse().unwrap_or(0))),
            ["F", name, size, mtime] if mine(name) => {
                if let Some((d, pre)) = owner(name).filter(|(d, _)| d == db) {
                    b.files.push(BackupFile { name: name.to_string(), db: d, size: size.parse().unwrap_or(0), mtime: mtime.parse().unwrap_or(0), pre });
                }
            }
            ["P", name, size] if mine(name) && owner(name).is_some_and(|(d, _)| d == db) => b.running.push((name.to_string(), size.parse().unwrap_or(0))),
            ["E", name, msg] if mine(name) && owner(name).is_some_and(|(d, _)| d == db) => b.failed.push((name.to_string(), msg.to_string())),
            ["R", d, state] if *d == db => b.restore = Some(state.to_string()),
            _ => {}
        }
    }
    b.files.sort_by(|a, c| c.mtime.cmp(&a.mtime).then(c.name.cmp(&a.name)));
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ct(name: &str, image: &str, state: &str) -> Container {
        Container { id: name.into(), name: name.into(), image: image.into(), state: state.into(), status: String::new(), ports: vec![], project: None, service: None, panel: None, mounts: vec![], app: None }
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

    #[test]
    fn starting_errors_are_recognized() {
        assert!(is_starting("ERROR 2002 (HY000): Can't connect to local MySQL server through socket '/var/run/mysqld/mysqld.sock' (2)"));
        assert!(is_starting("psql: error: connection to server on socket failed: FATAL:  the database system is starting up"));
        assert!(!is_starting("ERROR 1045 (28000): Access denied for user 'root'"));
    }

    #[test]
    fn conn_uri_escapes_and_masks() {
        assert_eq!(conn_uri(DbKind::Postgres, "postgres", "a/b:c@d", "10.0.0.1", "5432", false), "postgresql://postgres:a%2Fb%3Ac%40d@10.0.0.1:5432");
        assert_eq!(conn_uri(DbKind::Mysql, "root", "pw", "h", "3306", true), "mysql://root:••••@h:3306");
        assert_eq!(conn_uri(DbKind::Redis, "", "pw", "::1", "6379", false), "redis://:pw@[::1]:6379");
        assert_eq!(conn_uri(DbKind::Redis, "", "", "h", "6379", false), "redis://h:6379");
    }

    #[test]
    fn scripts_have_valid_shell_syntax() {
        for k in [DbKind::Postgres, DbKind::Mysql, DbKind::Redis] {
            for args in [Some(k.exec_args("c")), Some(k.conn_args("c"))].into_iter().flatten() {
                let script = args.last().unwrap();
                let ok = std::process::Command::new("sh").arg("-n").arg("-c").arg(script).status().unwrap().success();
                assert!(ok, "{k:?}: {script}");
            }
        }
        if let Ok(dir) = std::env::var("OTTER_DUMP_SCRIPTS") {
            for (n, k) in [("pg", DbKind::Postgres), ("my", DbKind::Mysql)] {
                std::fs::write(format!("{dir}/{n}.sh"), k.conn_args("c").last().unwrap()).unwrap();
                std::fs::write(format!("{dir}/{n}-client.sh"), k.exec_args("c").last().unwrap()).unwrap();
            }
            {
                let g = |db: &str, level, perms| Grant { db: db.into(), level, perms };
                let gs = [g("otg_a", Level::Read, 0), g("otg_b", Level::Write, 0), g("otg_c", Level::All, 0), g("otg_d", Level::Custom, P_SELECT | P_STRUCT)];
                for (n, k) in [("pg", DbKind::Postgres), ("my", DbKind::Mysql)] {
                    let mut q = String::new();
                    for d in ["otg_a", "otg_b", "otg_c", "otg_d"] {
                        q += &if n == "pg" { format!("CREATE DATABASE {d};\n") } else { format!("CREATE DATABASE {d};\n") };
                    }
                    q += &k.create_user_with_grants_sql("otg_u", "%", "Pw7-abc", &gs).unwrap();
                    std::fs::write(format!("{dir}/{n}-grants.sql"), q).unwrap();
                    let dbs: Vec<String> = ["otg_a", "otg_b", "otg_c", "otg_d"].iter().map(|s| s.to_string()).collect();
                    std::fs::write(format!("{dir}/{n}-read.sql"), k.read_grants_sql("otg_u", "%", &dbs).unwrap()).unwrap();
                    // 改成:a 全部、b 去掉、c 只读、d 不变
                    let new = [g("otg_a", Level::All, 0), g("otg_c", Level::Read, 0), g("otg_d", Level::Custom, P_SELECT | P_STRUCT)];
                    std::fs::write(format!("{dir}/{n}-update.sql"), k.update_grants_sql("otg_u", "%", &gs, &new).unwrap()).unwrap();
                }
            }
            let k = DbKind::Postgres;
            std::fs::write(format!("{dir}/pg-user.sql"), format!("CREATE DATABASE otpg_t;\n{}", k.create_user_sql("otpg_u", "", "Pw5-abc'x", "otpg_t").unwrap())).unwrap();
            std::fs::write(format!("{dir}/pg-user2.sql"), k.create_user_sql("otpg_plain", "", "Pw6-abc", "").unwrap()).unwrap();
            std::fs::write(format!("{dir}/pg-drop-u.sql"), k.drop_user_sql("otpg_u", "").unwrap()).unwrap();
            std::fs::write(format!("{dir}/pg-drop-plain.sql"), k.drop_user_sql("otpg_plain", "").unwrap()).unwrap();
            let k = DbKind::Mysql;
            std::fs::write(format!("{dir}/my-create.sql"), k.create_db_with_user_sql("otdb_full", "otuser_full", "Pw1-abc'\\x", true, "").unwrap()).unwrap();
            std::fs::write(format!("{dir}/my-create2.sql"), k.create_db_with_user_sql("otdb_lim", "otuser_lim", "Pw2-abc", false, "").unwrap()).unwrap();
            std::fs::write(format!("{dir}/my-user.sql"), k.create_user_sql("otuser_x", "%", "Pw3-abc", "otdb_lim").unwrap()).unwrap();
            std::fs::write(format!("{dir}/my-alter.sql"), k.alter_password_sql("otuser_x", "%", "Pw4-new'q").unwrap()).unwrap();
            std::fs::write(format!("{dir}/my-dropuser.sql"), format!("{}{}", k.drop_user_sql("otuser_x", "%").unwrap(), k.drop_user_sql("otuser_lim", "%").unwrap() + &k.drop_user_sql("otuser_full", "%").unwrap())).unwrap();
            std::fs::write(format!("{dir}/my-dropdb.sql"), format!("{}{}", k.drop_db_sql("otdb_full").unwrap(), k.drop_db_sql("otdb_lim").unwrap())).unwrap();
        }
    }

    #[test]
    fn grants_sql_levels() {
        let g = |db: &str, level, perms| Grant { db: db.into(), level, perms };
        let my = DbKind::Mysql.grants_sql("u", "%", &[g("a", Level::Read, 0), g("b", Level::Write, 0), g("c", Level::All, 0), g("d", Level::Custom, P_SELECT | P_STRUCT)]).unwrap();
        assert!(my.contains("GRANT SELECT, SHOW VIEW ON `a`.* TO 'u'@'%';"));
        assert!(my.contains("GRANT SELECT, INSERT, UPDATE, DELETE ON `b`.*"));
        assert!(my.contains("GRANT ALL PRIVILEGES ON `c`.*"));
        assert!(my.contains("GRANT SELECT, CREATE, ALTER, DROP, INDEX ON `d`.*"));
        let pg = DbKind::Postgres.grants_sql("u", "", &[g("a", Level::Read, 0), g("b", Level::All, 0)]).unwrap();
        assert!(pg.contains("GRANT CONNECT ON DATABASE \"a\" TO \"u\";"));
        assert!(pg.contains("GRANT SELECT ON ALL SEQUENCES IN SCHEMA public"));
        assert!(pg.contains("GRANT ALL PRIVILEGES ON DATABASE \"b\""));
        assert!(pg.matches("\\c ").count() == 2);
        // 自定义只勾「表结构」:不授表数据权限
        let only = DbKind::Postgres.grants_sql("u", "", &[g("a", Level::Custom, P_STRUCT)]).unwrap();
        assert!(only.contains("USAGE, CREATE ON SCHEMA") && !only.contains("ON ALL TABLES"));
        assert!(DbKind::Mysql.grants_sql("u", "%", &[g("a", Level::Custom, 0)]).is_err());
        assert!(DbKind::Mysql.grants_sql("u", "%", &[g("a`b", Level::Read, 0)]).is_err());
        assert!(DbKind::Mysql.grants_sql("u", "%", &[]).unwrap().is_empty());
    }

    #[test]
    fn parses_grants_and_diffs() {
        // MySQL:库 a 只读、b 读写、c 全部、d 自定义(查询+建表)、通配符库跳过
        let row = |db: &str, v: &str| format!("{db}\t{}\n", v.chars().map(|c| c.to_string()).collect::<Vec<_>>().join("\t"));
        let out = row("a", "YNNNNNNNYNNNNNNNNNN") + &row("b", "YYYYNNNNNNNNNNNNNNN") + &row("c", "YYYYYYYYYYYYYYYYYYN") + &row("d", "YNNNYNNNNNNNNNNNNNN") + &row("app\\_%", "YNNNNNNNNNNNNNNNNNN");
        let g = DbKind::Mysql.parse_grants(&out);
        assert_eq!(g.iter().map(|x| (x.db.as_str(), x.level)).collect::<Vec<_>>(), [("a", Level::Read), ("b", Level::Write), ("c", Level::All), ("d", Level::Custom)]);
        assert_eq!(g[3].perms, P_SELECT | P_STRUCT);
        // 带「可授权」的全部权限:档位还是「全部」,收回时多一句 REVOKE GRANT OPTION
        let go = DbKind::Mysql.parse_grants(&row("e", "YYYYYYYYYYYYYYYYYYY"));
        assert_eq!((go[0].level, go[0].perms & P_GRANTOPT), (Level::All, P_GRANTOPT));
        assert!(DbKind::Mysql.update_grants_sql("u", "%", &go, &[]).unwrap().contains("REVOKE GRANT OPTION ON `e`.*"));
        // PostgreSQL:没有显式授权的库不算
        let pg = DbKind::Postgres.parse_grants("a\tt\tf\tf\tt\tf\tf\tf\nb\tf\tf\tf\tt\tt\tt\tt\nc\tt\tt\tt\tt\tt\tt\tt\n");
        assert_eq!(pg.iter().map(|x| (x.db.as_str(), x.level)).collect::<Vec<_>>(), [("a", Level::Read), ("c", Level::All)]);
        // 差异:只收回变了的、只授新的
        let gr = |db: &str, level| Grant { db: db.into(), level, perms: 0 };
        let q = DbKind::Mysql.update_grants_sql("u", "%", &[gr("a", Level::Read), gr("b", Level::Write)], &[gr("a", Level::Read), gr("b", Level::All), gr("c", Level::Read)]).unwrap();
        assert!(q.contains("REVOKE ALL PRIVILEGES ON `b`.*"));
        assert!(!q.contains("`a`") && q.contains("GRANT ALL PRIVILEGES ON `b`.*") && q.contains("GRANT SELECT, SHOW VIEW ON `c`.*"));
        let pq = DbKind::Postgres.update_grants_sql("u", "", &[gr("a", Level::Read)], &[]).unwrap();
        assert!(pq.contains("REVOKE ALL ON DATABASE \"a\"") && pq.contains("ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON TABLES"));
    }

    #[test]
    fn classifies_addresses() {
        let (pri, publ) = classify_addrs("192.168.139.242\n10.0.0.5\n8.8.4.4\n172.17.0.1\nbad\n", "203.0.113.9");
        assert_eq!(pri, ["192.168.139.242", "10.0.0.5", "172.17.0.1"]);
        assert_eq!(publ, ["8.8.4.4", "203.0.113.9"]);
        let (pri, publ) = classify_addrs("", "db.example.com");
        assert!(pri.is_empty());
        assert_eq!(publ, ["db.example.com"]);
        // 节点地址是本机回环(如 OrbStack 的 127.0.0.1)不算
        assert!(classify_addrs("", "127.0.0.1").1.is_empty());
    }

    #[test]
    fn backup_scripts_are_valid_and_safe() {
        for k in [DbKind::Postgres, DbKind::Mysql, DbKind::Mariadb] {
            let ext = k.backup_ext().unwrap();
            let b = k.backup_script("my-db.1", "shop").unwrap();
            let r = k.restore_script("my-db.1", "shop", &format!("shop_20260101-101010.{ext}"), true).unwrap();
            let r0 = k.restore_script("my-db.1", "shop", &format!("shop_20260101-101010.{ext}"), false).unwrap();
            assert!(!r0.contains("-pre.") && !r0.contains("df -Pk") && r0.contains(".restore-shop"));
            for s in [&b, &r, &r0] {
                assert!(std::process::Command::new("sh").arg("-n").arg("-c").arg(s).status().unwrap().success(), "{k:?}: {s}");
                // 密码不会出现在脚本里,只在容器里读环境变量
                assert!(!s.contains("PASSWORD=") || s.contains("\"$p\""));
            }
            assert!(b.contains("databases/backups/my-db.1") && b.contains(&format!("shop_$(date +%Y%m%d-%H%M%S).{ext}")));
            assert!(r.contains("-pre.") && r.contains(".restore-shop"));
            if let Ok(dir) = std::env::var("OTTER_DUMP_SCRIPTS") {
                let n = if k == DbKind::Postgres { "pg" } else if k == DbKind::Mysql { "my" } else { "maria" };
                std::fs::write(format!("{dir}/bk-{n}.sh"), &b).unwrap();
                std::fs::write(format!("{dir}/rs-{n}.sh"), k.restore_script("my-db.1", "shop", &format!("shop_20260101-101010.{ext}"), true).unwrap()).unwrap();
                std::fs::write(format!("{dir}/rsn-{n}.sh"), &r0).unwrap();
                std::fs::write(format!("{dir}/ls.sh"), backup_list_script("my-db.1").unwrap()).unwrap();
                std::fs::write(format!("{dir}/cancel.sh"), backup_cancel_script("my-db.1", "shop_X.dump.part").unwrap()).unwrap();
            }
        }
        assert!(DbKind::Redis.backup_script("c", "x").is_err());
        assert!(DbKind::Postgres.backup_script("c;rm", "x").is_err());
        assert!(DbKind::Postgres.backup_script("c", "x y").is_err());
        // 恢复只接受属于这个库、扩展名对得上的文件
        assert!(DbKind::Postgres.restore_script("c", "shop", "other_20260101-101010.dump", false).is_err());
        assert!(DbKind::Postgres.restore_script("c", "shop", "shop_2026.sql.gz", false).is_err());
        assert!(DbKind::Postgres.restore_script("c", "shop", "../shop_1.dump", false).is_err());
        assert!(std::process::Command::new("sh").arg("-n").arg("-c").arg(backup_list_script("c").unwrap()).status().unwrap().success());
    }

    #[test]
    fn backup_file_names_are_validated() {
        assert!(valid_backup_file("shop_20260101-101010.dump") && valid_backup_file("a$b_1.sql.gz"));
        for bad in ["", "../x", "a/b", ".restore-x", "a b", "a;b", "a..b", "a\nb"] {
            assert!(!valid_backup_file(bad), "{bad}");
        }
        assert!(backup_cancel_script("c", "shop_1.dump.part").is_ok() && backup_cancel_script("c", "shop_1.dump").is_err() && backup_cancel_script("c", "../x.part").is_err());
        assert!(std::process::Command::new("sh").arg("-n").arg("-c").arg(backup_cancel_script("c", "shop_1.dump.part").unwrap()).status().unwrap().success());
        assert!(backup_delete_script("c", "x.part").is_err() && backup_delete_script("c", "x/y").is_err());
        assert_eq!(backup_remote_path("/opt/otter/databases/backups/c", "shop_1.dump").as_deref(), Some("/opt/otter/databases/backups/c/shop_1.dump"));
        assert!(backup_remote_path("rel", "shop_1.dump").is_none());
        assert!(backup_migrate_script("c").is_ok() && backup_migrate_script("a b").is_err());
        assert!(std::process::Command::new("sh").arg("-n").arg("-c").arg(backup_migrate_script("c").unwrap()).status().unwrap().success());
    }

    #[test]
    fn parses_backup_list() {
        let out = "H\t/home/u\nL\t3\t2048\nF\tshop_20260101-101010.dump\t2048\t1000\nF\tshop_20260102-101010-pre.dump\t4096\t2000\nF\tshop_old_20260101-101010.dump\t1\t1\nF\tother_20260101-101010.dump\t1\t1\nP\tshop_20260103-101010.dump.part\t99\nE\tshop_20260104-101010.dump.fail\tpg_dump: error\nR\tshop\trunning\nR\tother\tok\n";
        let b = parse_backups(out, "shop");
        assert_eq!(b.dir, "/home/u");
        assert_eq!(b.legacy, Some((3, 2048)));
        assert_eq!(b.files.iter().map(|f| (f.name.as_str(), f.pre)).collect::<Vec<_>>(), [("shop_20260102-101010-pre.dump", true), ("shop_20260101-101010.dump", false)]);
        assert_eq!(b.running, [("shop_20260103-101010.dump.part".to_string(), 99)]);
        assert_eq!(b.failed.len(), 1);
        assert_eq!(b.restore.as_deref(), Some("running"));
        // 库名里带下划线时不会和前缀相同的库混在一起
        assert_eq!(parse_backups(out, "shop_old").files.len(), 1);
    }
}
