//! 数据库的真机用例(PostgreSQL、MariaDB 容器):建库建用户、读库列表、备份 → 破坏数据 → 恢复(含恢复前备份)、
//! 定时备份脚本(保留份数只清自己的)、删库删用户。SQL 和 `docker exec` 的做法与 App 一致。

use super::harness::{Distro, Vm};
use crate::dbinst::*;
use crate::sysops::quote;

const RESET: &str = r#"
docker rm -f orbtest-pg orbtest-maria >/dev/null 2>&1
rm -rf /opt/otter/databases /etc/cron.d/otter-db-backup-*
true
"#;

struct Case {
    kind: DbKind,
    ctr: &'static str,
    image: &'static str,
    env: &'static str,
}

const PG: Case = Case { kind: DbKind::Postgres, ctr: "orbtest-pg", image: "postgres:16-alpine", env: "-e POSTGRES_PASSWORD=Orbtest-pw-1" };
const MARIA: Case = Case { kind: DbKind::Mariadb, ctr: "orbtest-maria", image: "mariadb:11", env: "-e MARIADB_ROOT_PASSWORD=Orbtest-pw-1 -e MYSQL_ROOT_PASSWORD=Orbtest-pw-1" };

/// 像 App 一样把 SQL 从 stdin 喂给容器里的客户端
fn sql(vm: &Vm, c: &Case, q: &str) -> (i32, String) {
    vm.run(&format!("docker exec -i {} sh -c {} <<'OTSQL'\n{q}\nOTSQL\n", c.ctr, quote(c.kind.client_script())))
}

fn sql_ok(vm: &Vm, c: &Case, q: &str) -> String {
    let (code, o) = sql(vm, c, q);
    assert_eq!(code, 0, "SQL 失败:{q}\n{o}");
    o
}

fn backups(vm: &Vm, c: &Case, db: &str) -> Backups {
    parse_backups(&vm.sh(&backup_list_script(c.ctr).unwrap()), db)
}

fn wait<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let t = std::time::Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t.elapsed().as_secs() < secs, "等不到:{what}");
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn run_case(d: Distro, c: Case) {
    let vm = Vm::shared(d, "otter", RESET);
    let k = c.kind;
    vm.pull(c.image);
    vm.exec("起容器", &format!("docker run -d --name {} {} {} 2>&1 | tail -1", c.ctr, c.env, c.image));
    wait("数据库就绪", 120, || sql(&vm, &c, "select 1;").0.eq(&0).then_some(()));

    // 建库 + 用户,读回
    let q = k.create_db_with_user_sql("shop", "shopper", "Shop-pw-1234", false, "").unwrap();
    sql_ok(&vm, &c, &q);
    let dbs = parse_dbs(k, &sql_ok(&vm, &c, k.list_dbs_sql()));
    assert!(dbs.iter().any(|d| d.name == "shop" && !d.system), "{dbs:?}");
    let users = parse_users(k, &sql_ok(&vm, &c, k.list_users_sql()));
    assert!(users.iter().any(|u| u.name == "shopper"), "{users:?}");
    // 非法名字被拒绝,不会拼进 SQL
    assert!(k.create_db_sql("a;drop", "").is_err());

    // 造数据
    let use_db = if k == DbKind::Postgres { "\\c shop\n" } else { "use shop;\n" };
    sql_ok(&vm, &c, &format!("{use_db}create table t(id int, v text);\ninsert into t values (1,'one'),(2,'两个'),(3,'three');\n"));
    let count = |vm: &Vm| sql_ok(vm, &c, &format!("{use_db}select count(*) from t;")).trim().to_string();
    assert_eq!(count(&vm), "3");

    // 立即备份 → 文件出现、非空
    let o = vm.exec("备份", &k.backup_script(c.ctr, "shop").unwrap());
    assert!(o.contains("started"), "{o}");
    let b = wait("备份完成", 90, || Some(backups(&vm, &c, "shop")).filter(|b| !b.files.is_empty() && b.running.is_empty()));
    assert!(b.failed.is_empty(), "{:?}", b.failed);
    let f = b.files[0].clone();
    assert!(f.size > 100 && !f.pre && !f.auto && f.name.ends_with(k.backup_ext().unwrap()), "{f:?}");

    // 破坏数据,再恢复(带恢复前备份)
    sql_ok(&vm, &c, &format!("{use_db}delete from t where id > 1;"));
    assert_eq!(count(&vm), "1");
    let o = vm.exec("恢复", &k.restore_script(c.ctr, "shop", &f.name, true).unwrap());
    assert!(o.contains("started"), "{o}");
    let b = wait("恢复完成", 90, || Some(backups(&vm, &c, "shop")).filter(|b| matches!(b.restore.as_deref(), Some(s) if s != "running")));
    assert_eq!(b.restore.as_deref(), Some("ok"), "恢复失败");
    assert_eq!(count(&vm), "3", "恢复后数据应回到备份时的 3 行");
    assert!(b.files.iter().any(|x| x.pre), "应有一份恢复前备份:{:?}", b.files);

    // 恢复一份不属于这个库的备份被拒绝;损坏的文件被拒绝
    assert!(k.restore_script(c.ctr, "shop", "other_20240101-000000.dump", false).is_err());
    if k != DbKind::Postgres {
        vm.exec("造损坏备份", &format!("d={}/backups/{}; echo not-gzip > $d/shop_20200101-000000.sql.gz", crate::srvdata::DB, c.ctr));
        let (code, o) = vm.run(&k.restore_script(c.ctr, "shop", "shop_20200101-000000.sql.gz", false).unwrap());
        assert!(code != 0 && o.contains("损坏"), "{code} {o}");
    }

    // 定时备份:装任务、手动跑两次,保留 1 份;手动备份不受影响
    let ext = k.backup_ext().unwrap();
    let id = "ab12cd";
    let body = k.sched_script(c.ctr, id, &["shop".to_string()], 1).unwrap();
    let cmd = sched_cmd("@@BIN@@", c.ctr, id, &["shop".to_string()], 1);
    vm.exec("装定时备份", &sched_install_root(c.ctr, id, &body, "30 2 * * *", &cmd).unwrap());
    let cron = vm.sh(&format!("cat {}", sched_crond_file(c.ctr, id)));
    let t = parse_sched(cron.split_once(" root ").unwrap().1.trim()).expect("解析不了任务行");
    assert_eq!((t.ctr.as_str(), t.id.as_str(), t.keep, t.dbs.as_slice()), (c.ctr, id, 1, ["shop".to_string()].as_slice()));
    for i in 0..2 {
        vm.exec(&format!("跑定时备份 第 {i} 次"), &format!("sh {}", t.script));
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    let b = backups(&vm, &c, "shop");
    let auto: Vec<_> = b.files.iter().filter(|x| x.auto).collect();
    assert_eq!(auto.len(), 1, "定时备份应只保留 1 份:{:?}", b.files);
    assert!(b.files.iter().any(|x| !x.auto && !x.pre && x.name == f.name), "手动备份不应被清理:{:?}", b.files);
    assert!(vm.sh(&format!("cat {}/backups/{}/.schedule.log", crate::srvdata::DB, c.ctr)).contains("备份完成"));
    let _ = ext;

    // 删备份
    let o = vm.exec("删备份", &backup_delete_script(c.ctr, &f.name).unwrap());
    assert!(!o.contains("ERR"), "{o}");
    assert!(!backups(&vm, &c, "shop").files.iter().any(|x| x.name == f.name));

    // 删库、删用户
    sql_ok(&vm, &c, &k.drop_db_sql("shop").unwrap());
    sql_ok(&vm, &c, &k.drop_user_sql("shopper", "%").unwrap());
    assert!(!parse_dbs(k, &sql_ok(&vm, &c, k.list_dbs_sql())).iter().any(|d| d.name == "shop"));
    assert!(!parse_users(k, &sql_ok(&vm, &c, k.list_users_sql())).iter().any(|u| u.name == "shopper"));
    vm.exec("清理容器", &format!("docker rm -f {}", c.ctr));
}

fn postgres(d: Distro) {
    run_case(d, PG)
}
fn mariadb(d: Distro) {
    run_case(d, MARIA)
}
per_distro!(postgres_flow, postgres);
per_distro!(mariadb_flow, mariadb);
