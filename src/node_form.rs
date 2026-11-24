//! 添加 / 编辑节点的弹窗表单。
//!
//! 添加和编辑共用(编辑由 `open_edit_node` 预填;密码、私钥等认证字段暂不回填)(标题、提交按钮文案由 `NodeForm::editing` 决定)。

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Home, MONO, c, icon, t};
use crate::ui::{Glass as _, GlassHover as _, GlassStyle, MenuItem, MenuSide, dropdown_menu, field, select};
use crate::mock::{Node, NodeId};

#[derive(Clone, PartialEq)]
pub enum TestState {
    Idle,
    Running,
    Ok(u32),
    Err(SharedString),
}

pub struct NodeForm {
    /// Some((分组, 节点)) = 编辑已有节点;None = 新增
    pub editing: Option<NodeId>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    key_path: Entity<InputState>,
    passphrase: Entity<InputState>,
    note: Entity<InputState>,
    tags: Entity<InputState>,
    /// 认证方式是叠加的(参考 Termius):用户名 + 密码始终显示,私钥、SSH Agent 按需添加
    use_key: bool,
    /// 私钥来源:true = 本机文件路径(旧方式),false = 钥匙串里的密钥(`key_id`)
    key_path_mode: bool,
    key_id: Option<i64>,
    /// 钥匙串密钥下拉是否展开
    key_menu: bool,
    use_agent: bool,
    show_pwd: bool,
    /// 「添加认证方式」下拉是否展开
    add_menu: bool,
    group: usize,
    /// 分组下拉是否展开
    group_menu: bool,
    test: TestState,
    /// 编辑时库里已有的认证信息
    existing_auth: Option<AuthRow>,
    /// 已保存的秘密读不出来(凭据库不可用等):保存时不动认证信息,避免把它们清掉
    auth_locked: bool,
    /// 点过提交后才显示必填项的错误态,避免一打开就满屏红框
    tried: bool,
}

impl NodeForm {
    pub fn new(group: usize, window: &mut Window, cx: &mut App) -> Self {
        let input = |placeholder: &'static str, value: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let form = Self {
            editing: None,
            name: input("例如 prod-web-01", "", window, cx),
            host: input("IP 或域名", "", window, cx),
            port: input("22", "22", window, cx),
            user: input("root", "root", window, cx),
            password: cx.new(|cx| InputState::new(window, cx).placeholder("SSH 密码").masked(true)),
            key_path: input("~/.ssh/id_ed25519", "~/.ssh/id_ed25519", window, cx),
            passphrase: cx.new(|cx| InputState::new(window, cx).placeholder("可选,没有可留空").masked(true)),
            note: input("备注(可选)", "", window, cx),
            tags: input("标签,用逗号分隔", "", window, cx),
            use_key: false,
            key_path_mode: false,
            key_id: None,
            key_menu: false,
            use_agent: false,
            show_pwd: false,
            add_menu: false,
            group,
            group_menu: false,
            test: TestState::Idle,
            tried: false,
            existing_auth: None,
            auth_locked: false,
        };
        let focus = form.host.focus_handle(cx);
        window.focus(&focus, cx);
        form
    }

    fn val(state: &Entity<InputState>, cx: &App) -> String {
        state.read(cx).value().trim().to_string()
    }
}

// ---- 认证信息的读写 -----------------------------------------------------------
use crate::store::{AuthRow, SLOT_KEY, SLOT_PASSPHRASE, SLOT_PASSWORD};
use zeroize::Zeroizing;

fn aad(id: NodeId, slot: &str) -> Vec<u8> {
    format!("{}:{slot}", id.0).into_bytes()
}

impl Home {
    pub(crate) fn ensure_vault(&mut self) -> Result<(), String> {
        if self.vault.is_none() {
            self.vault = Some(crate::secrets::Vault::open()?);
        }
        Ok(())
    }

    fn open_slot(&self, row: &AuthRow, id: NodeId, slot: &str) -> Result<Option<Zeroizing<String>>, String> {
        let Some((nonce, ct)) = row.secret(slot) else { return Ok(None) };
        let vault = self.vault.as_ref().ok_or("主密钥未就绪")?;
        let plain = vault.open_secret(nonce, ct, &aad(id, slot))?;
        String::from_utf8(plain.to_vec()).map(|s| Some(Zeroizing::new(s))).map_err(|_| "已保存的内容不是有效文本".to_string())
    }

    /// 连接用:节点地址 + 解密后的认证信息。读取失败不会 panic,错误放进 `auth_error` 在连接页显示。
    pub(crate) fn conn_for(&mut self, id: NodeId) -> crate::terminal::ssh::Conn {
        use crate::terminal::ssh::{Auth, Conn, KeyAuth};
        let mut conn = Conn::parse(&self.node_by_id(id).map(|n| n.user_host.clone()).unwrap_or_default());
        let Some(row) = self.store.as_ref().map(|s| s.load_auth(id)) else { return conn };
        let row = match row {
            Ok(r) => r,
            Err(e) => {
                conn.auth_error = Some(format!("读取认证信息失败:{e}"));
                return conn;
            }
        };
        // 还没保存过任何认证信息的旧节点保持原来的行为:SSH Agent
        if !row.use_agent && row.secrets.is_empty() && row.key_label.is_empty() && row.key_id.is_none() {
            return conn;
        }
        if !row.secrets.is_empty() || row.key_id.is_some() {
            if let Err(e) = self.ensure_vault() {
                conn.auth_error = Some(e);
                return conn;
            }
        }
        let read = || -> Result<Auth, String> {
            // 引用了钥匙串里的密钥就用它,否则用节点自己存的私钥
            let key = if let Some(kid) = row.key_id {
                Some(self.load_key_auth(kid)?)
            } else {
                match self.open_slot(&row, id, SLOT_KEY)? {
                    Some(pem) => Some(KeyAuth { pem, passphrase: self.open_slot(&row, id, SLOT_PASSPHRASE)?, label: row.key_label.clone() }),
                    None => None,
                }
            };
            Ok(Auth { key, agent: row.use_agent, password: self.open_slot(&row, id, SLOT_PASSWORD)? })
        };
        match read() {
            Ok(a) => conn.auth = a,
            Err(e) => conn.auth_error = Some(e),
        }
        conn
    }

    fn node_by_id(&self, id: NodeId) -> Option<&Node> {
        self.groups.iter().flat_map(|g| g.nodes.iter()).find(|n| n.id == id)
    }

    /// 从弹窗读出要保存的认证信息并加密。`existing` 是编辑时库里已有的(私钥路径没变就沿用旧密文)。
    fn build_auth(&self, id: NodeId, existing: Option<&AuthRow>, cx: &App) -> Result<AuthRow, String> {
        let f = self.form.as_ref().ok_or("弹窗已关闭")?;
        let seal = |slot: &str, plain: &[u8]| -> Result<(String, Vec<u8>, Vec<u8>), String> {
            let (n, c) = self.vault.as_ref().ok_or("主密钥未就绪")?.seal(plain, &aad(id, slot))?;
            Ok((slot.to_string(), n, c))
        };
        let mut row = AuthRow { use_agent: f.use_agent, ..Default::default() };
        if f.use_key && !f.key_path_mode {
            row.key_id = f.key_id;
        }
        let pw = f.password.read(cx).value().to_string();
        if !pw.is_empty() {
            row.secrets.push(seal(SLOT_PASSWORD, pw.as_bytes())?);
        }
        let path = NodeForm::val(&f.key_path, cx);
        if f.use_key && f.key_path_mode && !path.is_empty() {
            row.key_label = path.clone();
            let same = existing.filter(|e| e.key_label == path).and_then(|e| e.secret(SLOT_KEY).map(|(n, c)| (n.to_vec(), c.to_vec())));
            match same {
                Some((n, c)) => row.secrets.push((SLOT_KEY.into(), n, c)),
                None => {
                    let pem = std::fs::read_to_string(crate::terminal::ssh::expand(&path)).map_err(|e| format!("读取私钥 {path} 失败:{e}"))?;
                    match russh::keys::decode_secret_key(&pem, None) {
                        Ok(_) | Err(russh::keys::Error::KeyIsEncrypted) => {}
                        Err(e) => return Err(format!("{path} 不是有效的私钥:{e}")),
                    }
                    row.secrets.push(seal(SLOT_KEY, pem.as_bytes())?);
                }
            }
            let pass = f.passphrase.read(cx).value().to_string();
            if !pass.is_empty() {
                row.secrets.push(seal(SLOT_PASSPHRASE, pass.as_bytes())?);
            }
        }
        Ok(row)
    }

    /// 这次提交是否会写入秘密(决定要不要提前向系统凭据库取主密钥)
    fn form_has_secrets(&self, cx: &App) -> bool {
        self.form.as_ref().is_some_and(|f| !f.password.read(cx).value().is_empty() || (f.use_key && f.key_path_mode && !NodeForm::val(&f.key_path, cx).is_empty()))
    }
}

// ---- 交互 --------------------------------------------------------------------
impl Home {
    pub fn open_add_node(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (group, _) = self.locate(self.sel);
        self.form = Some(NodeForm::new(group, window, cx));
        cx.notify();
    }

    /// 打开编辑弹窗:用已有节点的值预填
    pub fn open_edit_node(&mut self, id: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let (gi, ni) = self.locate(id);
        let Some(n) = self.groups.get(gi).and_then(|g| g.nodes.get(ni)).filter(|n| n.id == id) else { return };
        let conn = crate::terminal::ssh::Conn::parse(&n.user_host);
        let (name, note, tags) = (n.name.clone(), n.note.clone(), n.tags.join(", "));
        let mut f = NodeForm::new(gi, window, cx);
        f.editing = Some(id);
        let set = |s: &Entity<InputState>, v: String, window: &mut Window, cx: &mut App| s.update(cx, |s, cx| s.set_value(v, window, cx));
        set(&f.name, name, window, cx);
        set(&f.host, conn.host, window, cx);
        set(&f.port, conn.port.to_string(), window, cx);
        set(&f.user, conn.user, window, cx);
        set(&f.note, note, window, cx);
        set(&f.tags, tags, window, cx);
        // 回显认证信息:密码、私钥路径、口令、是否用 Agent
        if let Some(row) = self.store.as_ref().map(|s| s.load_auth(id)) {
            match row {
                Ok(row) => {
                    f.use_agent = row.use_agent;
                    if row.key_id.is_some() {
                        f.use_key = true;
                        f.key_id = row.key_id;
                    } else if !row.key_label.is_empty() {
                        f.use_key = true;
                        f.key_path_mode = true;
                        set(&f.key_path, row.key_label.clone(), window, cx);
                    }
                    let mut err = None;
                    if !row.secrets.is_empty() {
                        err = self.ensure_vault().err();
                    }
                    if err.is_none() {
                        for (slot, input) in [(SLOT_PASSWORD, &f.password), (SLOT_PASSPHRASE, &f.passphrase)] {
                            match self.open_slot(&row, id, slot) {
                                Ok(Some(v)) => set(input, v.to_string(), window, cx),
                                Ok(None) => {}
                                Err(e) => err = Some(e),
                            }
                        }
                    }
                    if let Some(e) = err {
                        f.auth_locked = true;
                        f.test = TestState::Err(format!("无法读取已保存的密码/私钥:{e}。保存时不会改动它们").into());
                    }
                    f.existing_auth = Some(row);
                }
                Err(e) => {
                    f.auth_locked = true;
                    f.test = TestState::Err(format!("读取认证信息失败:{e}").into());
                }
            }
        }
        self.form = Some(f);
        cx.notify();
    }

    /// 调试:截图时直接显示钥匙串密钥选择行(DOCKYARD_FORM_KEY=1)
    pub(crate) fn debug_form_key(&mut self) {
        let first = self.keys.first().map(|k| k.id);
        if let Some(f) = &mut self.form {
            f.use_key = true;
            f.key_id = first;
            f.key_path_mode = first.is_none();
        }
    }

    /// 调试:截图时展开分组下拉(DOCKYARD_ADD_MENU=1)
    pub(crate) fn debug_open_group_menu(&mut self) {
        if let Some(f) = &mut self.form {
            // DOCKYARD_ADD_MENU=auth 展开「添加认证方式」,其他值展开分组下拉
            if std::env::var("DOCKYARD_ADD_MENU").as_deref() == Ok("auth") {
                f.add_menu = true;
            } else {
                f.group_menu = true;
            }
        }
    }

    /// 分组下拉:`Some(i)` = 选中第 i 个并收起;`None` = 切换展开
    fn set_group_menu(&mut self, pick: Option<usize>, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.form {
            match pick {
                Some(i) => {
                    f.group = i;
                    f.group_menu = false;
                }
                None => f.group_menu = !f.group_menu,
            }
            cx.notify();
        }
    }

    fn close_menus(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.form {
            f.group_menu = false;
            f.add_menu = false;
            f.key_menu = false;
            cx.notify();
        }
    }

    fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        cx.notify();
    }

    fn submit_form(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = &mut self.form else { return };
        f.tried = true;
        let name = NodeForm::val(&f.name, cx);
        let host = NodeForm::val(&f.host, cx);
        if name.is_empty() || host.is_empty() {
            cx.notify();
            return;
        }
        let user = NodeForm::val(&f.user, cx);
        let user = if user.is_empty() { "root".to_string() } else { user };
        let port = NodeForm::val(&f.port, cx).parse::<u16>().unwrap_or(22);
        let note = NodeForm::val(&f.note, cx);
        let tags: Vec<String> = NodeForm::val(&f.tags, cx).split(|c| c == ',' || c == '，').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
        let g = f.group.min(self.groups.len() - 1);
        let editing = f.editing;
        let locked = f.auth_locked;
        let existing = f.existing_auth.clone();
        let user_host = crate::store::user_host(&user, &host, port);
        // 有秘密要写时先取主密钥;取不到就整体失败,不写半截
        if self.store.is_some() && !locked && self.form_has_secrets(cx) {
            if let Err(e) = self.ensure_vault() {
                self.form_error(e, cx);
                return;
            }
        }
        // 先把私钥文件读好、校验好(失败不会留下半个节点)
        let pre = if self.store.is_some() && !locked && editing.is_some() { editing.map(|id| self.build_auth(id, existing.as_ref(), cx)) } else { None };
        if let Some(Err(e)) = &pre {
            self.form_error(e.clone(), cx);
            return;
        }
        let id = match editing {
            Some(id) => {
                if let Some(store) = &mut self.store {
                    let new = crate::store::NewNode { group_id: self.groups[g].id, name: &name, user: &user, host: &host, port, note: &note, tags: &tags };
                    if let Err(e) = store.update_node(id, &new) {
                        crate::logs::push(crate::logs::Level::Error, "本地库", &format!("保存节点失败: {e}"));
                        self.form_error(format!("保存失败:{e}"), cx);
                        return;
                    }
                }
                id
            }
            None => match &mut self.store {
                Some(store) => {
                    let new = crate::store::NewNode { group_id: self.groups[g].id, name: &name, user: &user, host: &host, port, note: &note, tags: &tags };
                    match store.add_node(&new) {
                        Ok(id) => id,
                        Err(e) => {
                            crate::logs::push(crate::logs::Level::Error, "本地库", &format!("保存节点失败: {e}"));
                            self.form_error(format!("保存失败:{e}"), cx);
                            return;
                        }
                    }
                }
                None => {
                    let id = NodeId(self.next_id);
                    self.next_id += 1;
                    id
                }
            },
        };
        // 认证信息(新增时要等拿到节点 id 才能加密,密文绑定了 id)
        if self.store.is_some() && !locked {
            let row = match pre {
                Some(Ok(r)) => Ok(r),
                _ => self.build_auth(id, existing.as_ref(), cx),
            };
            let saved = row.and_then(|r| self.store.as_mut().unwrap().save_auth(id, &r));
            if let Err(e) = saved {
                crate::logs::push(crate::logs::Level::Error, "本地库", &format!("保存认证信息失败: {e}"));
                // 新增时节点已经写进库了:先放进列表,避免重试产生重复
                if editing.is_none() {
                    self.groups[g].nodes.push(Node { id, name: name.clone(), user_host: user_host.clone(), note: note.clone(), tags: tags.clone(), stacks: vec![] });
                    self.form.as_mut().map(|f| f.editing = Some(id));
                }
                self.form_error(format!("节点已保存,但认证信息没存上:{e}"), cx);
                return;
            }
        }

        if editing.is_some() {
            let old_g = self.locate(id).0;
            if let Some(pos) = self.groups[old_g].nodes.iter().position(|n| n.id == id) {
                let mut node = self.groups[old_g].nodes.remove(pos);
                node.name = name;
                node.user_host = user_host;
                node.note = note;
                node.tags = tags;
                if old_g == g {
                    self.groups[g].nodes.insert(pos, node);
                } else {
                    self.groups[g].nodes.push(node);
                }
            }
        } else {
            self.groups[g].nodes.push(Node { id, name, user_host, note, tags, stacks: vec![] });
            self.sel = id;
            self.scroll = gpui_kit::component::VirtualListScrollHandle::new();
        }
        self.reload_keys();
        self.close_form(cx);
    }

    fn form_error(&mut self, msg: String, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.form {
            f.test = TestState::Err(msg.into());
        }
        cx.notify();
    }

    /// 模拟测试连接:等一会儿,主机为空或以 "bad" 开头则失败。
    fn test_connection(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &mut self.form else { return };
        let host = NodeForm::val(&f.host, cx);
        f.test = TestState::Running;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(f) = &mut this.form {
                    f.test = if host.is_empty() {
                        TestState::Err("请先填写主机地址".into())
                    } else if host.starts_with("bad") {
                        TestState::Err("连接超时".into())
                    } else {
                        TestState::Ok(30 + (host.len() as u32 * 17) % 200)
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn toggle_pwd(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = &mut self.form else { return };
        f.show_pwd = !f.show_pwd;
        let masked = !f.show_pwd;
        f.password.update(cx, |s, cx| s.set_masked(masked, window, cx));
        cx.notify();
    }

    fn browse_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择私钥".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(f) = &this.form {
                    f.key_path.update(cx, |s, cx| s.set_value(path.display().to_string(), window, cx));
                }
            });
        })
        .detach();
    }
}

// ---- 渲染 --------------------------------------------------------------------
// 布局:分组圆角容器(iOS 设置式),每行「左标签 + 右输入」,输入框不再各自带边框;
// 焦点用整行淡紫底表示,错误用整行淡红底 + 红标签。

/// 一行里的输入:无边框,撑满剩余宽度
pub(crate) fn input_cell(state: &Entity<InputState>, mono: bool) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .text_color(c::text())
        .text_size(px(t::TYPE_BODY))
        .when(mono, |d| d.font_family(MONO))
        // 组件按 size*0.875 算字号:换算成 13px 正文
        .child(Input::new(state).appearance(false).with_size(px(13.0 / 0.875)))
}

/// 小节:粗体标题 + 一组字段(节与节之间由调用方画分隔线)
pub(crate) fn section(title: &'static str, rows: Vec<Div>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).text_color(c::text()).child(title))
        .children(rows)
}

/// 行尾的小图标按钮(移除)
fn x_btn(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(t::RADIUS_SM))
        .cursor_pointer()
        .hover(|s| s.bg(c::hover()))
        .child(icon(IconName::X, 14.0, c::muted()))
}

/// 弹窗里的次要按钮:玻璃 CONTROL 样式
pub(crate) fn ctl_btn(id: &'static str, ic: Option<IconName>, text: &'static str, h: f32) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(h))
        .px(px(12.0))
        .glass(t::RADIUS_MD, GlassStyle::CONTROL)
        .glass_hover(GlassStyle::CONTROL)
        .text_size(px(t::TYPE_CONTROL))
        .text_color(c::secondary())
        .hover(|s| s.text_color(c::text()))
        .when_some(ic, |d, ic| d.child(icon(ic, 13.0, c::muted())))
        .child(text)
}

impl Home {
    pub fn node_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let f = self.form.as_ref()?;
        let name_err = f.tried && NodeForm::val(&f.name, cx).is_empty();
        let host_err = f.tried && NodeForm::val(&f.host, cx).is_empty();
        let focus = |s: &Entity<InputState>, cx: &Context<Self>| s.focus_handle(cx).is_focused(window);

        // 分组:下拉选择
        let this = cx.entity();
        let (t1, t2, t3) = (this.clone(), this.clone(), this);
        let group_select = select(
            "group-menu",
            IconName::Folder,
            self.groups.iter().map(|g| MenuItem::new(g.name.clone()).color(g.color)).collect(),
            f.group.min(self.groups.len() - 1),
            f.group_menu,
            move |_, cx| t1.update(cx, |h, cx| h.set_group_menu(None, cx)),
            move |i, _, cx| t2.update(cx, |h, cx| h.set_group_menu(Some(i), cx)),
            move |_, cx| t3.update(cx, |h, cx| h.close_menus(cx)),
        );

        // ---- 连接 ----
        let conn = section(
            "连接",
            vec![
                field(IconName::Globe, focus(&f.host, cx), host_err, input_cell(&f.host, true)),
                field(IconName::Server, focus(&f.name, cx), name_err, input_cell(&f.name, false)),
            ],
        );

        // ---- 认证 ----
        let eye = div()
            .id("toggle-pwd")
            .size(px(28.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(t::RADIUS_SM))
            .cursor_pointer()
            .hover(|s| s.bg(c::hover()))
            .child(icon(if f.show_pwd { IconName::EyeOff } else { IconName::Eye }, 15.0, c::muted()))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_pwd(window, cx)));
        let mut cred_rows: Vec<Div> = vec![
            field(IconName::Hash, focus(&f.port, cx), false, div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(8.0)).child(div().text_size(px(t::TYPE_BODY)).text_color(c::muted()).child("SSH 端口")).child(input_cell(&f.port, true))),
            field(IconName::User, focus(&f.user, cx), false, input_cell(&f.user, true)),
            field(
                IconName::LockKeyhole,
                focus(&f.password, cx),
                false,
                div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(4.0)).child(input_cell(&f.password, false)).child(eye),
            ),
        ];
        if f.use_key && !f.key_path_mode {
            // 从钥匙串选一把密钥;最后一项切到「本机文件」的旧方式
            let mut items: Vec<MenuItem> = self.keys.iter().map(|k| MenuItem::new(k.name.clone()).sub(k.algo.trim_start_matches("ssh-").to_string()).icon(IconName::KeyRound)).collect();
            items.push(MenuItem::new("使用密钥文件…").sub("直接读取本机的私钥文件").icon(IconName::FileKey));
            let n_keys = self.keys.len();
            let ids: Vec<i64> = self.keys.iter().map(|k| k.id).collect();
            let sel = f.key_id.and_then(|id| ids.iter().position(|&x| x == id)).unwrap_or(n_keys);
            let (k1, k2, k3) = (cx.entity(), cx.entity(), cx.entity());
            let sel_el = select(
                "key-select",
                IconName::KeyRound,
                items,
                sel,
                f.key_menu,
                move |_, cx| k1.update(cx, |h, cx| { if let Some(f) = &mut h.form { f.key_menu = !f.key_menu; cx.notify(); } }),
                move |i, _, cx| {
                    k2.update(cx, |h, cx| {
                        if let Some(f) = &mut h.form {
                            if i < n_keys { f.key_id = Some(ids[i]) } else { f.key_id = None; f.key_path_mode = true }
                            f.key_menu = false;
                            cx.notify();
                        }
                    })
                },
                move |_, cx| k3.update(cx, |h, cx| h.close_menus(cx)),
            );
            cred_rows.push(div().flex().flex_row().items_center().gap(px(6.0)).child(div().flex_1().min_w_0().child(sel_el)).child(x_btn("rm-key").on_click(cx.listener(|this, _, _, cx| {
                if let Some(f) = &mut this.form {
                    f.use_key = false;
                    f.key_id = None;
                    cx.notify();
                }
            }))));
        } else if f.use_key {
            cred_rows.push(field(
                IconName::KeyRound,
                focus(&f.key_path, cx),
                false,
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(input_cell(&f.key_path, true))
                    .child(ctl_btn("browse-key", Some(IconName::FolderOpen), "浏览", 28.0).on_click(cx.listener(|this, _, window, cx| this.browse_key(window, cx))))
                    .child(x_btn("rm-key").on_click(cx.listener(|this, _, _, cx| {
                        if let Some(f) = &mut this.form {
                            f.use_key = false;
                            cx.notify();
                        }
                    }))),
            ));
            cred_rows.push(field(IconName::Lock, focus(&f.passphrase, cx), false, input_cell(&f.passphrase, false)));
        }
        if f.use_agent {
            cred_rows.push(field(
                IconName::FingerprintPattern,
                false,
                false,
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().flex_1().text_size(px(t::TYPE_BODY)).text_color(c::muted()).child("使用本机 ssh-agent 中的密钥"))
                    .child(x_btn("rm-agent").on_click(cx.listener(|this, _, _, cx| {
                        if let Some(f) = &mut this.form {
                            f.use_agent = false;
                            cx.notify();
                        }
                    }))),
            ));
        }

        // 「+ 添加私钥、SSH Agent」链接,点开下拉(向上展开,盖在表单上)
        let mut remaining: Vec<&'static str> = vec![];
        if !f.use_key {
            remaining.push("私钥");
        }
        if !f.use_agent {
            remaining.push("SSH Agent");
        }
        let add_link = (!remaining.is_empty()).then(|| {
            let menu = f.add_menu.then(|| {
                let mut items = vec![];
                let mut kinds = vec![];
                if !f.use_key {
                    items.push(MenuItem::new("私钥").sub("使用钥匙串里的密钥或本机的私钥文件").icon(IconName::KeyRound));
                    kinds.push(true);
                }
                if !f.use_agent {
                    items.push(MenuItem::new("SSH Agent").sub("使用本机 ssh-agent 中的密钥").icon(IconName::FingerprintPattern));
                    kinds.push(false);
                }
                let (t1, t2) = (cx.entity(), cx.entity());
                dropdown_menu(
                    "add-auth-menu",
                    items,
                    None,
                    MenuSide::Above,
                    38.0,
                    move |i, _, cx| {
                        let key = kinds[i];
                        t1.update(cx, |h, cx| {
                            if let Some(f) = &mut h.form {
                                if key {
                                    f.use_key = true;
                                    // 钥匙串里有密钥就默认选第一把,没有就退回填文件路径
                                    f.key_path_mode = h.keys.is_empty();
                                    f.key_id = h.keys.first().map(|k| k.id);
                                } else {
                                    f.use_agent = true
                                }
                                f.add_menu = false;
                                cx.notify();
                            }
                        })
                    },
                    move |_, cx| t2.update(cx, |h, cx| h.close_menus(cx)),
                )
            });
            // 外层是横向容器,里面的可点元素只占文字宽度,点右边空白不触发
            div()
                .relative()
                .w(px(crate::ui::DROPDOWN_W))
                .flex()
                .flex_row()
                .child(
                    div()
                        .id("add-auth")
                        .h(px(30.0))
                        .px(px(4.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .rounded(px(t::RADIUS_SM))
                        .cursor_pointer()
                        .text_size(px(t::TYPE_CONTROL))
                        .text_color(c::ghost())
                        .hover(|s| s.text_color(c::secondary()))
                        .child(icon(IconName::Plus, 14.0, c::muted()))
                        .child(format!("添加 {}", remaining.join("、")))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(f) = &mut this.form {
                                f.add_menu = true;
                                cx.notify();
                            }
                        })),
                )
                .children(menu)
        });
        let cred = section("SSH 认证", cred_rows).children(add_link);
        let general = section(
            "通用",
            vec![
                group_select,
                field(IconName::Tag, focus(&f.tags, cx), false, input_cell(&f.tags, false)),
                field(IconName::FileText, focus(&f.note, cx), false, input_cell(&f.note, false)),
            ],
        );

        let body = div().flex().flex_col().gap(px(18.0)).px(px(24.0)).pt(px(4.0)).pb(px(22.0)).child(conn).child(div().h(px(1.0)).bg(c::line())).child(general).child(div().h(px(1.0)).bg(c::line())).child(cred);

        let test_result = match &f.test {
            TestState::Idle => None,
            TestState::Running => Some((IconName::LoaderCircle, c::muted(), "正在连接…".to_string())),
            TestState::Ok(ms) => Some((IconName::CircleCheck, c::online(), format!("连接成功 · {ms}ms"))),
            TestState::Err(e) => Some((IconName::CircleX, c::danger(), e.to_string())),
        };
        let can_submit = !NodeForm::val(&f.name, cx).is_empty() && !NodeForm::val(&f.host, cx).is_empty();

        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .child(ctl_btn("test-conn", Some(IconName::Zap), "测试连接", 34.0).on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))))
            .when_some(test_result, |d, (ic, color, text)| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(5.0))
                        .text_size(px(t::TYPE_CONTROL))
                        .text_color(color)
                        .child(icon(ic, 13.0, color))
                        .child(text),
                )
            })
            .child(div().flex_1())
            .child(ctl_btn("cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| this.close_form(cx))))
            .child(
                div()
                    .id("submit")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(34.0))
                    .px(px(16.0))
                    .rounded(px(t::RADIUS_MD))
                    .text_size(px(t::TYPE_CONTROL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .when(can_submit, |d| d.bg(c::accent()).text_color(c::on_accent()).hover(|s| s.opacity(0.88)))
                    .when(!can_submit, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()).cursor_default())
                    .child(icon(IconName::Plus, 13.0, if can_submit { c::on_accent() } else { c::ghost() }))
                    .child(if f.editing.is_some() { "保存" } else { "添加节点" })
                    .on_click(cx.listener(|this, _, window, cx| this.submit_form(window, cx))),
            );

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.0))
            .pl(px(24.0))
            .pr(px(52.0)) // 给右上角的关闭按钮让位
            .pt(px(24.0))
            .pb(px(18.0))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(px(t::TYPE_TITLE_LG))
                            .font_weight(FontWeight::BOLD)
                            .child(if f.editing.is_some() { "编辑节点" } else { "添加节点" }),
                    )
                    .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("通过 SSH 连接一台运行 Docker 的服务器")),
            );

        let close = div()
            .id("close-form")
            .absolute()
            .top(px(14.0))
            .right(px(14.0))
            .size(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(t::RADIUS_SM))
            .cursor_pointer()
            .hover(|s| s.bg(c::hover()))
            .child(icon(IconName::X, 14.0, c::muted()))
            .on_click(cx.listener(|this, _, _, cx| this.close_form(cx)));

        Some(crate::ui::modal(
            "node-dialog",
            440.0,
            cx.listener(|this, e: &KeyDownEvent, window, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    match this.form.as_mut() {
                        Some(f) if f.add_menu || f.group_menu || f.key_menu => {
                            f.key_menu = false;
                            f.add_menu = false;
                            f.group_menu = false;
                            cx.notify();
                        }
                        _ => this.close_form(cx),
                    }
                    cx.stop_propagation();
                }
                "enter" => {
                    this.submit_form(window, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [
                header.into_any_element(),
                div().id("form-body").flex_1().min_h_0().overflow_y_scroll().child(body).into_any_element(),
                footer.into_any_element(),
                close.into_any_element(),
            ],
        ))
    }
}

// ---- 端口悬停提示 ------------------------------------------------------------
pub struct PortsTip(pub Vec<String>);

impl Render for PortsTip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(10.0))
            .rounded(px(t::RADIUS_MD))
            .border_1()
            .border_color(c::border_strong())
            .bg(c::raised_hi())
            .shadow_lg()
            .child(
                div()
                    .text_size(px(t::TYPE_CAPTION))
                    .text_color(c::ghost())
                    .child(format!("全部端口 · {}", self.0.len())),
            )
            .children(self.0.iter().map(|p| {
                div()
                    .text_size(px(t::TYPE_CAPTION))
                    .font_family(MONO)
                    .text_color(c::secondary())
                    .child(p.clone())
            }))
    }
}
