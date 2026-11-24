//! 本地配置库(SQLite):分组、节点、标签、备注。
//!
//! 文件位于各平台的标准数据目录(见 `data_dir`)。迁移用 `PRAGMA user_version`,
//! 每个版本一段 SQL,启动时按序执行。堆栈、容器来自服务器,不在这里存。
//! 秘密(私钥、口令、密码)另见计划 `2026-11-23-node-storage`,尚未接入。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::mock::{Group, Node, NodeId};

/// 各平台的数据目录:macOS `~/Library/Application Support/otter`、
/// Windows `%APPDATA%\otter`、Linux `$XDG_DATA_HOME/otter`。
pub fn data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "otter").map(|d| d.data_dir().to_path_buf())
}

/// 展示用的 `user@host`,端口不是 22 时带 `:port`。
pub fn user_host(user: &str, host: &str, port: u16) -> String {
    if port == 22 { format!("{user}@{host}") } else { format!("{user}@{host}:{port}") }
}

/// 新增节点所需的字段。
pub struct NewNode<'a> {
    pub group_id: i64,
    pub name: &'a str,
    pub user: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub note: &'a str,
    pub tags: &'a [String],
}

pub struct Store {
    conn: Connection,
}

/// 导入一个节点所需的全部内容(分组按名字找,没有就建);秘密是明文,由 `import_nodes` 加密后入库。
pub struct ImportItem {
    pub group: String,
    pub name: String,
    pub user: String,
    pub host: String,
    pub port: u16,
    pub note: String,
    pub tags: Vec<String>,
    pub use_agent: bool,
    pub key_id: Option<i64>,
    pub key_label: String,
    /// (槽位, 明文)
    pub secrets: Vec<(String, zeroize::Zeroizing<String>)>,
}

/// 导入后每个节点落在哪个分组、拿到什么 id(顺序与传入的 `ImportItem` 一致)。
pub struct Imported {
    pub group_id: i64,
    pub group_name: String,
    pub color: u32,
    pub id: NodeId,
}

/// 导入时新建分组用的颜色(与默认分组相同)
const IMPORT_GROUP_COLOR: u32 = 8228518;

/// 迁移脚本,下标 + 1 = 目标版本。只追加,不改已发布的版本。
const MIGRATIONS: &[&str] = &[
    // v1:分组、节点、标签,并放一个默认分组(空库时表单的分组下拉也有得选)
    "CREATE TABLE groups (
         id       INTEGER PRIMARY KEY,
         name     TEXT NOT NULL,
         color    INTEGER NOT NULL,
         position INTEGER NOT NULL DEFAULT 0
     );
     CREATE TABLE nodes (
         id         INTEGER PRIMARY KEY,
         group_id   INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
         name       TEXT NOT NULL,
         user       TEXT NOT NULL,
         host       TEXT NOT NULL,
         port       INTEGER NOT NULL DEFAULT 22,
         note       TEXT NOT NULL DEFAULT '',
         position   INTEGER NOT NULL DEFAULT 0,
         created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
     );
     CREATE TABLE node_tags (
         node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
         tag     TEXT NOT NULL,
         PRIMARY KEY (node_id, tag)
     );
     INSERT INTO groups (name, color, position) VALUES ('未分组', 8228518, 0);",
    // v2:节点的认证选择和加密后的秘密(密码、私钥、口令)
    "CREATE TABLE node_auth (
         node_id   INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
         use_agent INTEGER NOT NULL DEFAULT 0,
         key_label TEXT NOT NULL DEFAULT ''
     );
     CREATE TABLE secrets (
         node_id    INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
         slot       TEXT NOT NULL,
         nonce      BLOB NOT NULL,
         ciphertext BLOB NOT NULL,
         PRIMARY KEY (node_id, slot)
     );",
    // v3:界面偏好(本地栏上次所在目录、排序方式等),键值对
    "CREATE TABLE settings (
         key   TEXT PRIMARY KEY,
         value TEXT NOT NULL
     );",
    // v4:钥匙串里的密钥对(元数据)和加密后的私钥/口令;节点可以引用其中一把
    "CREATE TABLE keys (
         id             INTEGER PRIMARY KEY,
         name           TEXT NOT NULL,
         algo           TEXT NOT NULL,
         public_key     TEXT NOT NULL,
         fingerprint    TEXT NOT NULL,
         has_passphrase INTEGER NOT NULL DEFAULT 0,
         source         TEXT NOT NULL DEFAULT '',
         created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
     );
     CREATE TABLE key_secrets (
         key_id     INTEGER NOT NULL REFERENCES keys(id) ON DELETE CASCADE,
         slot       TEXT NOT NULL,
         nonce      BLOB NOT NULL,
         ciphertext BLOB NOT NULL,
         PRIMARY KEY (key_id, slot)
     );
     ALTER TABLE node_auth ADD COLUMN key_id INTEGER REFERENCES keys(id) ON DELETE SET NULL;",
];

/// 秘密槽位
pub const SLOT_PASSWORD: &str = "password";
pub const SLOT_KEY: &str = "private_key";
pub const SLOT_PASSPHRASE: &str = "passphrase";

/// 节点认证的存储形态:密文由调用方加解密。
#[derive(Default, Clone)]
pub struct AuthRow {
    pub use_agent: bool,
    /// 私钥的来源标签(导入时的文件路径,仅用于显示)
    pub key_label: String,
    /// 引用的钥匙串密钥(有它就不用节点自己的私钥)
    pub key_id: Option<i64>,
    /// (槽位, nonce, 密文)
    pub secrets: Vec<(String, Vec<u8>, Vec<u8>)>,
}

/// 钥匙串里的一把密钥(不含私钥)
#[derive(Clone, Debug, PartialEq)]
pub struct KeyRow {
    pub id: i64,
    pub name: String,
    pub algo: String,
    /// OpenSSH 单行格式
    pub public_key: String,
    pub fingerprint: String,
    pub has_passphrase: bool,
    /// 来源:导入的文件路径,或「生成」
    pub source: String,
    pub used_by: usize,
}

pub struct NewKey<'a> {
    pub name: &'a str,
    pub algo: &'a str,
    pub public_key: &'a str,
    pub fingerprint: &'a str,
    pub has_passphrase: bool,
    pub source: &'a str,
}

/// 钥匙串密钥的秘密槽位:只有私钥 PEM(钥匙串不保存口令);沿用节点的槽位名
pub const KEY_SLOT_PRIVATE: &str = SLOT_KEY;

impl AuthRow {
    pub fn secret(&self, slot: &str) -> Option<(&[u8], &[u8])> {
        self.secrets.iter().find(|(s, _, _)| s == slot).map(|(_, n, c)| (n.as_slice(), c.as_slice()))
    }
}

impl Store {
    /// 打开(必要时创建)默认位置的库。
    pub fn open_default() -> Result<Self, String> {
        let dir = data_dir().ok_or("找不到数据目录")?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
        Self::open(&dir.join("otter.db"))
    }

    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("打开数据库失败: {e}"))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self, String> {
        Self::init(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn init(conn: Connection) -> Result<Self, String> {
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;").map_err(|e| e.to_string())?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn user_version(&self) -> Result<i64, String> {
        self.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| e.to_string())
    }

    fn migrate(&self) -> Result<(), String> {
        let cur = self.user_version()? as usize;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(cur) {
            // 每个版本一个事务:脚本和版本号一起生效,中途失败不留半截
            let tx = format!("BEGIN; {sql} PRAGMA user_version = {}; COMMIT;", i + 1);
            self.conn.execute_batch(&tx).map_err(|e| {
                let _ = self.conn.execute_batch("ROLLBACK;");
                format!("数据库迁移到 v{} 失败: {e}", i + 1)
            })?;
        }
        Ok(())
    }

    /// 读界面偏好,没有或出错返回 None
    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).ok()
    }

    /// 写界面偏好(覆盖)
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.conn
            .execute("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [key, value])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// 读出全部分组和它们的节点(堆栈为空,由服务器实时提供)。
    pub fn load_groups(&self) -> Result<Vec<Group>, String> {
        let e = |e: rusqlite::Error| e.to_string();
        let mut groups: Vec<Group> = self
            .conn
            .prepare("SELECT id, name, color FROM groups ORDER BY position, id")
            .map_err(e)?
            .query_map([], |r| Ok(Group { id: r.get(0)?, name: r.get(1)?, color: r.get::<_, u32>(2)?, nodes: vec![] }))
            .map_err(e)?
            .collect::<Result<_, _>>()
            .map_err(e)?;

        let mut tags: std::collections::HashMap<i64, Vec<String>> = Default::default();
        let mut st = self.conn.prepare("SELECT node_id, tag FROM node_tags ORDER BY tag").map_err(e)?;
        for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).map_err(e)? {
            let (id, tag) = row.map_err(e)?;
            tags.entry(id).or_default().push(tag);
        }

        let mut st = self.conn.prepare("SELECT id, group_id, name, user, host, port, note FROM nodes ORDER BY position, id").map_err(e)?;
        let rows = st
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, u16>(5)?, r.get::<_, String>(6)?))
            })
            .map_err(e)?;
        for row in rows {
            let (id, gid, name, user, host, port, note) = row.map_err(e)?;
            if let Some(g) = groups.iter_mut().find(|g| g.id == gid) {
                g.nodes.push(Node {
                    id: NodeId(id as u32),
                    name,
                    user_host: user_host(&user, &host, port),
                    note,
                    tags: tags.remove(&id).unwrap_or_default(),
                    stacks: vec![],
                });
            }
        }
        Ok(groups)
    }

    /// 整体覆盖节点的认证选择和秘密(没出现的槽位会被删除)。
    pub fn save_auth(&mut self, id: NodeId, a: &AuthRow) -> Result<(), String> {
        let e = |e: rusqlite::Error| e.to_string();
        let tx = self.conn.transaction().map_err(e)?;
        tx.execute(
            "INSERT INTO node_auth (node_id, use_agent, key_label, key_id) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(node_id) DO UPDATE SET use_agent=excluded.use_agent, key_label=excluded.key_label, key_id=excluded.key_id",
            params![id.0, a.use_agent, a.key_label, a.key_id],
        )
        .map_err(e)?;
        tx.execute("DELETE FROM secrets WHERE node_id=?1", params![id.0]).map_err(e)?;
        for (slot, nonce, ct) in &a.secrets {
            tx.execute("INSERT INTO secrets (node_id, slot, nonce, ciphertext) VALUES (?1, ?2, ?3, ?4)", params![id.0, slot, nonce, ct]).map_err(e)?;
        }
        tx.commit().map_err(e)
    }

    pub fn load_auth(&self, id: NodeId) -> Result<AuthRow, String> {
        let e = |e: rusqlite::Error| e.to_string();
        let mut row = AuthRow::default();
        if let Some((agent, label, key_id)) = self
            .conn
            .query_row("SELECT use_agent, key_label, key_id FROM node_auth WHERE node_id=?1", params![id.0], |r| {
                Ok((r.get::<_, bool>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?))
            })
            .optional()
            .map_err(e)?
        {
            row.use_agent = agent;
            row.key_label = label;
            row.key_id = key_id;
        }
        let mut st = self.conn.prepare("SELECT slot, nonce, ciphertext FROM secrets WHERE node_id=?1").map_err(e)?;
        row.secrets = st
            .query_map(params![id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(e)?
            .collect::<Result<_, _>>()
            .map_err(e)?;
        Ok(row)
    }

    // ---- 钥匙串 ----

    /// 所有密钥(按添加顺序),带被几个节点引用。
    pub fn list_keys(&self) -> Result<Vec<KeyRow>, String> {
        let e = |e: rusqlite::Error| e.to_string();
        let mut st = self
            .conn
            .prepare(
                "SELECT k.id, k.name, k.algo, k.public_key, k.fingerprint, k.has_passphrase, k.source,
                        (SELECT COUNT(*) FROM node_auth a WHERE a.key_id = k.id)
                 FROM keys k ORDER BY k.id",
            )
            .map_err(e)?;
        st.query_map([], |r| {
            Ok(KeyRow {
                id: r.get(0)?,
                name: r.get(1)?,
                algo: r.get(2)?,
                public_key: r.get(3)?,
                fingerprint: r.get(4)?,
                has_passphrase: r.get(5)?,
                source: r.get(6)?,
                used_by: r.get::<_, i64>(7)? as usize,
            })
        })
        .map_err(e)?
        .collect::<Result<_, _>>()
        .map_err(e)
    }

    /// 新增密钥。密文绑定了密钥 id,所以先插入拿到 id,再由 `seal` 生成密文;任何一步失败整体回滚。
    pub fn add_key(&mut self, k: &NewKey, seal: impl FnOnce(i64) -> Result<Vec<(String, Vec<u8>, Vec<u8>)>, String>) -> Result<i64, String> {
        let e = |e: rusqlite::Error| e.to_string();
        let tx = self.conn.transaction().map_err(e)?;
        tx.execute(
            "INSERT INTO keys (name, algo, public_key, fingerprint, has_passphrase, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![k.name, k.algo, k.public_key, k.fingerprint, k.has_passphrase, k.source],
        )
        .map_err(e)?;
        let id = tx.last_insert_rowid();
        for (slot, nonce, ct) in seal(id)? {
            tx.execute("INSERT INTO key_secrets (key_id, slot, nonce, ciphertext) VALUES (?1, ?2, ?3, ?4)", params![id, slot, nonce, ct]).map_err(e)?;
        }
        tx.commit().map_err(e)?;
        Ok(id)
    }

    pub fn key_secrets(&self, id: i64) -> Result<Vec<(String, Vec<u8>, Vec<u8>)>, String> {
        let e = |e: rusqlite::Error| e.to_string();
        let mut st = self.conn.prepare("SELECT slot, nonce, ciphertext FROM key_secrets WHERE key_id=?1").map_err(e)?;
        st.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(e)?.collect::<Result<_, _>>().map_err(e)
    }

    pub fn rename_key(&self, id: i64, name: &str) -> Result<(), String> {
        let n = self.conn.execute("UPDATE keys SET name=?1 WHERE id=?2", params![name, id]).map_err(|e| e.to_string())?;
        if n == 0 { Err("密钥不存在".into()) } else { Ok(()) }
    }

    /// 删除密钥;引用它的节点的 `key_id` 自动置空。
    pub fn delete_key(&self, id: i64) -> Result<(), String> {
        self.conn.execute("DELETE FROM keys WHERE id=?1", params![id]).map(|_| ()).map_err(|e| e.to_string())
    }

    /// 把节点移到另一个分组(拖拽用),不动其他字段。
    pub fn move_node(&self, id: NodeId, group_id: i64) -> Result<(), String> {
        let n = self.conn.execute("UPDATE nodes SET group_id=?1 WHERE id=?2", params![group_id, id.0]).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("节点不存在".into());
        }
        Ok(())
    }

    /// 删除节点(标签、认证、秘密随外键级联删除)。
    pub fn delete_node(&self, id: NodeId) -> Result<(), String> {
        let n = self.conn.execute("DELETE FROM nodes WHERE id=?1", params![id.0]).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("节点不存在".into());
        }
        Ok(())
    }

    /// 删除分组及其下所有节点(外键级联)。
    pub fn delete_group(&self, id: i64) -> Result<(), String> {
        let n = self.conn.execute("DELETE FROM groups WHERE id=?1", params![id]).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("分组不存在".into());
        }
        Ok(())
    }

    /// 更新节点(含换分组、重写标签)。
    pub fn update_node(&mut self, id: NodeId, n: &NewNode) -> Result<(), String> {
        let e = |e: rusqlite::Error| e.to_string();
        let tx = self.conn.transaction().map_err(e)?;
        let rows = tx
            .execute(
                "UPDATE nodes SET group_id=?1, name=?2, user=?3, host=?4, port=?5, note=?6 WHERE id=?7",
                params![n.group_id, n.name, n.user, n.host, n.port, n.note, id.0],
            )
            .map_err(e)?;
        if rows == 0 {
            return Err("节点不存在".into());
        }
        tx.execute("DELETE FROM node_tags WHERE node_id=?1", params![id.0]).map_err(e)?;
        for t in n.tags {
            tx.execute("INSERT OR IGNORE INTO node_tags (node_id, tag) VALUES (?1, ?2)", params![id.0, t]).map_err(e)?;
        }
        tx.commit().map_err(e)
    }

    /// 批量导入节点:一个事务里建分组、写节点、标签、认证和加密后的秘密,任何一步失败都整体回滚。
    /// `seal(节点 id, 槽位, 明文)` 返回 (nonce, 密文);密文绑定节点 id,所以要等拿到 id 才能加密。
    pub fn import_nodes(
        &mut self,
        items: &[ImportItem],
        seal: impl Fn(NodeId, &str, &[u8]) -> Result<(Vec<u8>, Vec<u8>), String>,
    ) -> Result<Vec<Imported>, String> {
        let e = |e: rusqlite::Error| e.to_string();
        let tx = self.conn.transaction().map_err(e)?;
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let found: Option<i64> = tx.query_row("SELECT id FROM groups WHERE name=?1 ORDER BY position, id LIMIT 1", params![it.group], |r| r.get(0)).optional().map_err(e)?;
            let gid = match found {
                Some(id) => id,
                None => {
                    tx.execute(
                        "INSERT INTO groups (name, color, position) VALUES (?1, ?2, (SELECT COALESCE(MAX(position), 0) + 1 FROM groups))",
                        params![it.group, IMPORT_GROUP_COLOR],
                    )
                    .map_err(e)?;
                    tx.last_insert_rowid()
                }
            };
            let color: u32 = tx.query_row("SELECT color FROM groups WHERE id=?1", params![gid], |r| r.get(0)).map_err(e)?;
            tx.execute(
                "INSERT INTO nodes (group_id, name, user, host, port, note, position)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, (SELECT COALESCE(MAX(position), 0) + 1 FROM nodes))",
                params![gid, it.name, it.user, it.host, it.port, it.note],
            )
            .map_err(e)?;
            let id = tx.last_insert_rowid();
            for t in &it.tags {
                tx.execute("INSERT OR IGNORE INTO node_tags (node_id, tag) VALUES (?1, ?2)", params![id, t]).map_err(e)?;
            }
            if it.use_agent || it.key_id.is_some() || !it.key_label.is_empty() || !it.secrets.is_empty() {
                tx.execute("INSERT INTO node_auth (node_id, use_agent, key_label, key_id) VALUES (?1, ?2, ?3, ?4)", params![id, it.use_agent, it.key_label, it.key_id]).map_err(e)?;
            }
            let node_id = NodeId(id as u32);
            for (slot, plain) in &it.secrets {
                let (nonce, ct) = seal(node_id, slot, plain.as_bytes())?;
                tx.execute("INSERT INTO secrets (node_id, slot, nonce, ciphertext) VALUES (?1, ?2, ?3, ?4)", params![id, slot, nonce, ct]).map_err(e)?;
            }
            out.push(Imported { group_id: gid, group_name: it.group.clone(), color, id: node_id });
        }
        tx.commit().map_err(e)?;
        Ok(out)
    }

    pub fn add_node(&mut self, n: &NewNode) -> Result<NodeId, String> {
        let e = |e: rusqlite::Error| e.to_string();
        let tx = self.conn.transaction().map_err(e)?;
        tx.execute(
            "INSERT INTO nodes (group_id, name, user, host, port, note, position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, (SELECT COALESCE(MAX(position), 0) + 1 FROM nodes))",
            params![n.group_id, n.name, n.user, n.host, n.port, n.note],
        )
        .map_err(e)?;
        let id = tx.last_insert_rowid();
        for t in n.tags {
            tx.execute("INSERT OR IGNORE INTO node_tags (node_id, tag) VALUES (?1, ?2)", params![id, t]).map_err(e)?;
        }
        tx.commit().map_err(e)?;
        Ok(NodeId(id as u32))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn settings_roundtrip() {
        let st = Store::open_memory().unwrap();
        assert_eq!(st.get_setting("k"), None);
        st.set_setting("k", "a").unwrap();
        st.set_setting("k", "b").unwrap();
        assert_eq!(st.get_setting("k").as_deref(), Some("b"));
    }

    use super::*;

    #[test]
    fn migrates_to_latest_and_has_default_group() {
        let s = Store::open_memory().unwrap();
        assert_eq!(s.user_version().unwrap(), MIGRATIONS.len() as i64);
        let groups = s.load_groups().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "未分组");
        assert!(groups[0].nodes.is_empty());
    }

    #[test]
    fn move_node_changes_group_only() {
        let mut s = Store::open_memory().unwrap();
        let g1 = s.load_groups().unwrap()[0].id;
        s.conn.execute("INSERT INTO groups (name, color, position) VALUES ('b', 0, 1)", []).unwrap();
        let g2 = s.conn.last_insert_rowid();
        let id = s.add_node(&NewNode { group_id: g1, name: "x", user: "u", host: "h", port: 22, note: "", tags: &[] }).unwrap();
        s.move_node(id, g2).unwrap();
        let gs = s.load_groups().unwrap();
        assert!(gs.iter().find(|g| g.id == g1).unwrap().nodes.is_empty());
        assert_eq!(gs.iter().find(|g| g.id == g2).unwrap().nodes[0].name, "x");
        assert!(s.move_node(NodeId(999), g2).is_err());
    }

    #[test]
    fn delete_node_and_group_cascade() {
        let mut s = Store::open_memory().unwrap();
        let g1 = s.load_groups().unwrap()[0].id;
        s.conn.execute("INSERT INTO groups (name, color, position) VALUES ('b', 0, 1)", []).unwrap();
        let g2 = s.conn.last_insert_rowid();
        let tags = vec!["t".to_string()];
        let a = s.add_node(&NewNode { group_id: g1, name: "a", user: "u", host: "h", port: 22, note: "", tags: &tags }).unwrap();
        s.add_node(&NewNode { group_id: g2, name: "b", user: "u", host: "h2", port: 22, note: "", tags: &tags }).unwrap();
        s.delete_node(a).unwrap();
        assert!(s.delete_node(a).is_err());
        s.delete_group(g2).unwrap();
        let gs = s.load_groups().unwrap();
        assert!(gs.iter().all(|g| g.id != g2));
        assert!(gs.iter().all(|g| g.nodes.is_empty()));
        let left: i64 = s.conn.query_row("SELECT COUNT(*) FROM node_tags", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0);
        assert!(s.delete_group(g2).is_err());
    }

    #[test]
    fn update_node_changes_fields_and_tags() {
        let mut s = Store::open_memory().unwrap();
        let gid = s.load_groups().unwrap()[0].id;
        let tags = vec!["a".to_string(), "b".to_string()];
        let id = s.add_node(&NewNode { group_id: gid, name: "x", user: "root", host: "h", port: 22, note: "", tags: &tags }).unwrap();
        let tags2 = vec!["c".to_string()];
        s.update_node(id, &NewNode { group_id: gid, name: "y", user: "u", host: "h2", port: 2222, note: "n", tags: &tags2 }).unwrap();
        let n = &s.load_groups().unwrap()[0].nodes[0];
        assert_eq!((n.name.as_str(), n.user_host.as_str(), n.note.as_str(), n.tags.clone()), ("y", "u@h2:2222", "n", vec!["c".to_string()]));
        assert!(s.update_node(NodeId(999), &NewNode { group_id: gid, name: "z", user: "u", host: "h", port: 22, note: "", tags: &[] }).is_err());
    }

    #[test]
    fn auth_roundtrip_overwrite_and_cascade() {
        let mut s = Store::open_memory().unwrap();
        let gid = s.load_groups().unwrap()[0].id;
        let id = s.add_node(&NewNode { group_id: gid, name: "x", user: "root", host: "h", port: 22, note: "", tags: &[] }).unwrap();
        assert!(s.load_auth(id).unwrap().secrets.is_empty());
        let a = AuthRow { use_agent: true, key_label: "~/k".into(), key_id: None, secrets: vec![(SLOT_PASSWORD.into(), vec![1], vec![2]), (SLOT_KEY.into(), vec![3], vec![4])] };
        s.save_auth(id, &a).unwrap();
        let got = s.load_auth(id).unwrap();
        assert!(got.use_agent);
        assert_eq!(got.key_label, "~/k");
        assert_eq!(got.secret(SLOT_PASSWORD), Some((&[1u8][..], &[2u8][..])));
        s.save_auth(id, &AuthRow::default()).unwrap();
        let got = s.load_auth(id).unwrap();
        assert!(!got.use_agent && got.secrets.is_empty());
        s.save_auth(id, &a).unwrap();
        s.conn.execute("DELETE FROM nodes WHERE id=?1", params![id.0]).unwrap();
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "删除节点要级联删除秘密");
    }

    #[test]
    fn keys_crud_usage_and_cascade() {
        let mut s = Store::open_memory().unwrap();
        let gid = s.load_groups().unwrap()[0].id;
        let node = s.add_node(&NewNode { group_id: gid, name: "n", user: "root", host: "h", port: 22, note: "", tags: &[] }).unwrap();
        let k = NewKey { name: "k1", algo: "ssh-ed25519", public_key: "ssh-ed25519 AAAA", fingerprint: "SHA256:x", has_passphrase: false, source: "生成" };
        // seal 失败要整体回滚
        assert!(s.add_key(&k, |_| Err("boom".into())).is_err());
        assert!(s.list_keys().unwrap().is_empty());
        let id = s.add_key(&k, |id| Ok(vec![(SLOT_KEY.into(), vec![id as u8], vec![9])])).unwrap();
        assert_eq!(s.key_secrets(id).unwrap().len(), 1);
        s.save_auth(node, &AuthRow { key_id: Some(id), ..Default::default() }).unwrap();
        assert_eq!(s.list_keys().unwrap()[0].used_by, 1);
        assert_eq!(s.load_auth(node).unwrap().key_id, Some(id));
        s.rename_key(id, "k2").unwrap();
        assert_eq!(s.list_keys().unwrap()[0].name, "k2");
        s.delete_key(id).unwrap();
        assert!(s.list_keys().unwrap().is_empty());
        assert!(s.key_secrets(id).unwrap().is_empty());
        assert_eq!(s.load_auth(node).unwrap().key_id, None);
    }

    #[test]
    fn migrate_is_idempotent() {
        let s = Store::open_memory().unwrap();
        s.migrate().unwrap();
        assert_eq!(s.load_groups().unwrap().len(), 1);
    }

    #[test]
    fn add_and_load_node_with_tags() {
        let mut s = Store::open_memory().unwrap();
        let gid = s.load_groups().unwrap()[0].id;
        let tags = vec!["web".to_string(), "prod".to_string(), "web".to_string()];
        let id = s.add_node(&NewNode { group_id: gid, name: "a", user: "root", host: "10.0.0.1", port: 2222, note: "备注", tags: &tags }).unwrap();
        let groups = s.load_groups().unwrap();
        let n = &groups[0].nodes[0];
        assert_eq!(n.id, id);
        assert_eq!(n.user_host, "root@10.0.0.1:2222");
        assert_eq!(n.note, "备注");
        assert_eq!(n.tags, vec!["prod".to_string(), "web".to_string()]); // 去重并按字母序
    }

    #[test]
    fn nodes_keep_insert_order_and_ids_are_unique() {
        let mut s = Store::open_memory().unwrap();
        let gid = s.load_groups().unwrap()[0].id;
        let mk = |s: &mut Store, name| s.add_node(&NewNode { group_id: gid, name, user: "u", host: "h", port: 22, note: "", tags: &[] }).unwrap();
        let (a, b) = (mk(&mut s, "a"), mk(&mut s, "b"));
        assert_ne!(a, b);
        let names: Vec<_> = s.load_groups().unwrap()[0].nodes.iter().map(|n| n.name.clone()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn data_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("otter-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let _ = std::fs::remove_file(&path);
        {
            let mut s = Store::open(&path).unwrap();
            let gid = s.load_groups().unwrap()[0].id;
            s.add_node(&NewNode { group_id: gid, name: "keep", user: "root", host: "1.2.3.4", port: 22, note: "", tags: &[] }).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.load_groups().unwrap()[0].nodes[0].name, "keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn item(group: &str, name: &str, secrets: Vec<(String, zeroize::Zeroizing<String>)>) -> ImportItem {
        ImportItem { group: group.into(), name: name.into(), user: "root".into(), host: format!("{name}.example.com"), port: 2222, note: "n".into(), tags: vec!["a".into(), "b".into()], use_agent: true, key_id: None, key_label: String::new(), secrets }
    }

    #[test]
    fn import_creates_groups_nodes_and_auth() {
        let mut s = Store::open_memory().unwrap();
        let v = crate::secrets::Vault::for_test();
        let seal = |id: NodeId, slot: &str, p: &[u8]| v.seal(p, &crate::node_form::aad(id, slot));
        let items = vec![
            item("未分组", "a", vec![(SLOT_PASSWORD.into(), zeroize::Zeroizing::new("pw".into()))]),
            item("Project/Dev", "b", vec![]),
            item("Project/Dev", "c", vec![]),
        ];
        let got = s.import_nodes(&items, seal).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].group_id, got[2].group_id, "同名新分组只建一次");
        let groups = s.load_groups().unwrap();
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["未分组", "Project/Dev"]);
        assert_eq!(groups[1].nodes.len(), 2);
        assert_eq!(groups[0].nodes[0].user_host, "root@a.example.com:2222");
        assert_eq!(groups[0].nodes[0].tags, ["a", "b"]);
        let auth = s.load_auth(got[0].id).unwrap();
        assert!(auth.use_agent);
        let (n, c) = auth.secret(SLOT_PASSWORD).unwrap();
        assert_eq!(&*v.open_secret(n, c, &crate::node_form::aad(got[0].id, SLOT_PASSWORD)).unwrap(), b"pw");
    }

    #[test]
    fn import_rolls_back_on_failure() {
        let mut s = Store::open_memory().unwrap();
        let items = vec![item("新分组", "a", vec![]), item("新分组", "b", vec![(SLOT_PASSWORD.into(), zeroize::Zeroizing::new("pw".into()))])];
        let res = s.import_nodes(&items, |_, _, _| Err("加密失败".into()));
        assert!(res.is_err());
        let groups = s.load_groups().unwrap();
        assert_eq!(groups.len(), 1, "新建的分组也要回滚");
        assert!(groups[0].nodes.is_empty());
    }
}
