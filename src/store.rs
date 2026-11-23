//! 本地配置库(SQLite):分组、节点、标签、备注。
//!
//! 文件位于各平台的标准数据目录(见 `data_dir`)。迁移用 `PRAGMA user_version`,
//! 每个版本一段 SQL,启动时按序执行。堆栈、容器来自服务器,不在这里存。
//! 秘密(私钥、口令、密码)另见计划 `2026-11-23-node-storage`,尚未接入。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::mock::{Group, Node, NodeId};

/// 各平台的数据目录:macOS `~/Library/Application Support/dockyard`、
/// Windows `%APPDATA%\dockyard`、Linux `$XDG_DATA_HOME/dockyard`。
pub fn data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "dockyard").map(|d| d.data_dir().to_path_buf())
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
    /// (槽位, nonce, 密文)
    pub secrets: Vec<(String, Vec<u8>, Vec<u8>)>,
}

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
        Self::open(&dir.join("dockyard.db"))
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
            "INSERT INTO node_auth (node_id, use_agent, key_label) VALUES (?1, ?2, ?3)
             ON CONFLICT(node_id) DO UPDATE SET use_agent=excluded.use_agent, key_label=excluded.key_label",
            params![id.0, a.use_agent, a.key_label],
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
        if let Some((agent, label)) = self
            .conn
            .query_row("SELECT use_agent, key_label FROM node_auth WHERE node_id=?1", params![id.0], |r| Ok((r.get::<_, bool>(0)?, r.get::<_, String>(1)?)))
            .optional()
            .map_err(e)?
        {
            row.use_agent = agent;
            row.key_label = label;
        }
        let mut st = self.conn.prepare("SELECT slot, nonce, ciphertext FROM secrets WHERE node_id=?1").map_err(e)?;
        row.secrets = st
            .query_map(params![id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(e)?
            .collect::<Result<_, _>>()
            .map_err(e)?;
        Ok(row)
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
        let a = AuthRow { use_agent: true, key_label: "~/k".into(), secrets: vec![(SLOT_PASSWORD.into(), vec![1], vec![2]), (SLOT_KEY.into(), vec![3], vec![4])] };
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
        let dir = std::env::temp_dir().join(format!("dockyard-test-{}", std::process::id()));
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
}
