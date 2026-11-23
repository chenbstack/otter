//! 首页用的 mock 数据。全部是确定性生成,没有任何真实连接。

pub const SPARK_N: usize = 12;

#[derive(Clone, Copy, PartialEq)]
pub enum Running {
    Up,
    Down,
}

pub struct Service {
    pub name: String,
    pub image: String,
    pub state: Running,
    pub uptime: String,
    pub cpu: String,
    /// 数值版,用于堆栈 / 节点级汇总
    pub cpu_v: f32,
    pub mem_mb: f32,
    pub mem: String,
    pub net: String,
    pub disk: String,
    pub cpu_s: [f32; SPARK_N],
    pub mem_s: [f32; SPARK_N],
    pub net_s: [f32; SPARK_N],
    pub disk_s: [f32; SPARK_N],
    pub ports: Vec<String>,
}

pub struct Stack {
    pub name: String,
    pub services: Vec<Service>,
    pub expanded: bool,
}

impl Stack {
    pub fn running(&self) -> usize {
        self.services.iter().filter(|s| s.state == Running::Up).count()
    }
}

/// 节点稳定 id:标签、选中、文件状态都用它引用节点,不用(分组下标, 节点下标)。0 保留为"无"。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NodeId(pub u32);

pub struct Node {
    pub id: NodeId,
    pub name: String,
    pub user_host: String,
    pub stacks: Vec<Stack>,
}

impl Node {
    pub fn service_count(&self) -> usize {
        self.stacks.iter().map(|s| s.services.len()).sum()
    }
}

pub struct Group {
    pub name: String,
    pub color: u32,
    pub nodes: Vec<Node>,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn f(&mut self) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0
    }
    fn spark(&mut self, base: f32) -> [f32; SPARK_N] {
        let mut s = [0.0; SPARK_N];
        let mut v = base;
        for x in &mut s {
            v = (v + (self.f() - 0.5) * 0.5).clamp(0.08, 1.0);
            *x = v;
        }
        s
    }
}

pub fn fmt_mem(mb: f32) -> String {
    if mb > 800.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
}

fn svc(r: &mut Rng, name: &str, image: &str, up: bool, ports: &[&str]) -> Service {
    let cpu = r.f() * 4.0;
    let mem = 10.0 + r.f() * 900.0;
    Service {
        name: name.into(),
        image: image.into(),
        state: if up { Running::Up } else { Running::Down },
        uptime: if up {
            format!("{}天 {}小时", 1 + r.next() % 60, r.next() % 24)
        } else {
            "已停止".into()
        },
        cpu: format!("{cpu:.1}%"),
        cpu_v: cpu,
        mem_mb: mem,
        mem: fmt_mem(mem),
        net: format!("{:.1} MB/s", r.f() * 30.0),
        disk: format!("{:.1} GB/s", r.f() * 5.0),
        cpu_s: r.spark(0.3),
        mem_s: r.spark(0.6),
        net_s: r.spark(0.4),
        disk_s: r.spark(0.5),
        ports: ports.iter().map(|p| p.to_string()).collect(),
    }
}

fn stack(name: &str, services: Vec<Service>) -> Stack {
    Stack { name: name.into(), services, expanded: false }
}

/// emby 节点:和旧版截图里的内容一致,方便对比外观。
fn emby(r: &mut Rng) -> Vec<Stack> {
    vec![
        stack("admin", vec![svc(r, "watchtower", "containrrr/watchtower", true, &["8080/tcp"])]),
        stack("ddns-go", vec![svc(r, "ddns-go", "jeessy/ddns-go:latest", true, &["9876/tcp"])]),
        stack(
            "emby",
            vec![
                svc(r, "emby_av", "amilys/embyserver:latest", true, &["1900/udp", "7359/udp", "8920/tcp", "0.0.0.0:8097->8096/tcp"]),
                svc(r, "emby_4.8", "amilys/embyserver:4.8.11.0", true, &["1900/udp", "7359/udp", "0.0.0.0:8096->8096/tcp"]),
            ],
        ),
        stack(
            "openresty",
            vec![svc(r, "web-openresty-a1b2", "1panel/openresty:1.27.1.2-5-1-focal", true, &["80/tcp", "443/tcp"])],
        ),
        stack(
            "root",
            vec![
                svc(r, "mysql", "mysql:8.0", true, &["3306/tcp"]),
                svc(r, "redis", "redis:7-alpine", false, &["6379/tcp"]),
            ],
        ),
    ]
}

/// 大节点:用来看长列表滚动。
fn big(r: &mut Rng, n: usize) -> Vec<Stack> {
    const IMAGES: [&str; 6] = ["nginx:1.27", "postgres:16", "redis:7", "grafana/grafana:11", "traefik:v3", "minio/minio:latest"];
    (0..n)
        .map(|i| {
            let count = 1 + (r.next() % 6) as usize;
            let services = (0..count)
                .map(|j| {
                    let img = IMAGES[(r.next() % 6) as usize];
                    let up = r.next() % 9 != 0;
                    svc(r, &format!("app{i:02}-svc{j}"), img, up, &["8080/tcp", "9090/tcp"])
                })
                .collect();
            stack(&format!("project-{i:02}"), services)
        })
        .collect()
}

pub fn groups() -> Vec<Group> {
    let mut r = Rng(0x9E3779B97F4A7C15);
    let test = vec![
        stack("whoami", vec![svc(&mut r, "whoami", "traefik/whoami", true, &["80/tcp"])]),
        stack(
            "grafana",
            vec![
                svc(&mut r, "grafana", "grafana/grafana:11", true, &["3000/tcp"]),
                svc(&mut r, "prometheus", "prom/prometheus", true, &["9090/tcp"]),
            ],
        ),
    ];
    let emby_stacks = emby(&mut r);
    let docker_host = big(&mut r, 6);
    let cluster = big(&mut r, 60);
    let sg = big(&mut r, 4);
    let mut groups = vec![
        Group {
            name: "开发分组".into(),
            color: 0x6b9dff,
            nodes: vec![Node { id: NodeId(0), name: "test".into(), user_host: "dev@192.0.2.30".into(), stacks: test }],
        },
        Group {
            name: "生产分组".into(),
            color: 0x4ce3a0,
            nodes: vec![
                Node { id: NodeId(0), name: "emby".into(), user_host: "deploy@192.0.2.22".into(), stacks: emby_stacks },
                Node { id: NodeId(0), name: "docker-host".into(), user_host: "deploy@192.0.2.21".into(), stacks: docker_host },
                Node { id: NodeId(0), name: "big-cluster".into(), user_host: "ops@198.51.100.21".into(), stacks: cluster },
                Node { id: NodeId(0), name: "hk-edge-01".into(), user_host: "root@198.51.100.8".into(), stacks: vec![] },
            ],
        },
        Group {
            name: "未分组".into(),
            color: 0x7d8ea6,
            nodes: vec![Node { id: NodeId(0), name: "sg-gateway".into(), user_host: "ubuntu@203.0.113.2".into(), stacks: sg }],
        },
    ];
    // 按出现顺序分配 id,从 1 开始
    let mut next = 1;
    for n in groups.iter_mut().flat_map(|g| g.nodes.iter_mut()) {
        n.id = NodeId(next);
        next += 1;
    }
    groups
}
