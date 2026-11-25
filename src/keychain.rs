//! 钥匙串里的密钥对:解析、生成、指纹。不碰数据库和界面。

use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey, ssh_key::LineEnding};
use zeroize::Zeroizing;

/// 解析出来的密钥信息(不含私钥)
#[derive(Debug, PartialEq)]
pub struct Parsed {
    pub algo: String,
    /// OpenSSH 单行格式
    pub public_key: String,
    pub fingerprint: String,
    /// 私钥文件本身是加密的
    pub encrypted: bool,
}

fn describe(public: &PublicKey, encrypted: bool) -> Result<Parsed, String> {
    let mut p = public.clone();
    p.set_comment("");
    Ok(Parsed {
        algo: public.algorithm().to_string(),
        public_key: p.to_openssh().map_err(|e| e.to_string())?,
        fingerprint: public.fingerprint(HashAlg::Sha256).to_string(),
        encrypted,
    })
}

/// 解析私钥文本,取出算法、公钥和指纹。加密的私钥文件头里本来就带公钥,所以也能识别(`encrypted` 为真)。
pub fn inspect(pem: &str) -> Result<Parsed, String> {
    let key = PrivateKey::from_openssh(pem.trim().as_bytes()).map_err(|e| format!("不是有效的 OpenSSH 私钥:{e}"))?;
    describe(key.public_key(), key.is_encrypted())
}

/// 用户另外粘贴的公钥必须和私钥配对。
pub fn check_public(parsed: &Parsed, public_text: &str) -> Result<(), String> {
    let public = PublicKey::from_openssh(public_text.trim()).map_err(|e| format!("公钥格式不对:{e}"))?;
    if public.fingerprint(HashAlg::Sha256).to_string() != parsed.fingerprint {
        return Err("公钥和私钥不匹配".into());
    }
    Ok(())
}

/// 生成一对 Ed25519 密钥(私钥不加密)。返回私钥 PEM 和解析信息。
pub fn generate_ed25519(comment: &str) -> Result<(Zeroizing<String>, Parsed), String> {
    let mut rng = rand::rng();
    let mut key = PrivateKey::random(&mut rng, Algorithm::Ed25519).map_err(|e| format!("生成密钥失败:{e}"))?;
    key.set_comment(comment);
    let parsed = describe(key.public_key(), false)?;
    let pem = key.to_openssh(LineEnding::LF).map_err(|e| e.to_string())?;
    Ok((pem, parsed))
}

// ---- 与界面状态、本地库、加密器的衔接 ------------------------------------------------
use crate::terminal::ssh::KeyAuth;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{InputState, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::node_form::{ctl_btn, input_cell};
use crate::ui::{Glass as _, GlassStyle, empty_state, field, tag};
use crate::{Home, MONO, c, icon, store, t};

/// 钥匙串密钥的加密绑定:密文搬到别的密钥或节点上解不开
pub fn key_aad(id: i64, slot: &str) -> Vec<u8> {
    format!("key:{id}:{slot}").into_bytes()
}

impl Home {
    /// 取私钥 + 口令,用于连接。
    pub(crate) fn load_key_auth(&self, id: i64) -> Result<KeyAuth, String> {
        let store = self.store.as_ref().ok_or("本地库不可用")?;
        let vault = self.vault.as_ref().ok_or("主密钥未就绪")?;
        let rows = store.key_secrets(id)?;
        let open = |slot: &str| -> Result<Option<Zeroizing<String>>, String> {
            let Some((_, n, c)) = rows.iter().find(|(s, _, _)| s == slot) else { return Ok(None) };
            let plain = vault.open_secret(n, c, &key_aad(id, slot))?;
            String::from_utf8(plain.to_vec()).map(|s| Some(Zeroizing::new(s))).map_err(|_| "已保存的内容不是有效文本".to_string())
        };
        let pem = open(store::KEY_SLOT_PRIVATE)?.ok_or("钥匙串里的这把密钥缺少私钥(可能已被删除)")?;
        let label = self.keys.iter().find(|k| k.id == id).map(|k| k.name.clone()).unwrap_or_default();
        Ok(KeyAuth { pem, passphrase: None, label })
    }
}

/// 添加密钥弹窗里私钥框、公钥框、拖入文件区的统一高度
const KEY_AREA_H: f32 = 112.0;

// ---- 状态 ------------------------------------------------------------------

/// 钥匙串页的弹窗
pub enum KeyModal {
    Add(KeyDialog),
    Rename { id: i64, input: Entity<InputState> },
    Delete { id: i64 },
}

/// 添加密钥弹窗:导入已有私钥,或生成新的
pub struct KeyDialog {
    generate: bool,
    name: Entity<InputState>,
    private: Entity<TextareaState>,
    public: Entity<TextareaState>,
    error: Option<SharedString>,
    tried: bool,
}

fn val(s: &Entity<InputState>, cx: &App) -> String {
    s.read(cx).value().trim().to_string()
}

impl Home {
    /// 重新读钥匙串列表(节点改了引用、增删密钥后调用)
    pub(crate) fn reload_keys(&mut self) {
        if let Some(Ok(keys)) = self.store.as_ref().map(|s| s.list_keys()) {
            self.keys = keys;
        }
    }

    /// 调试:截图时直接打开钥匙串弹窗(OTTER_KEY_MODAL=add|gen|rename|delete)
    pub(crate) fn debug_key_modal(&mut self, kind: &str, window: &mut Window, cx: &mut Context<Self>) {
        match kind {
            "rename" => self.open_rename_key(0, window, cx),
            "delete" => self.key_modal = Some(KeyModal::Delete { id: self.keys.get(1).map_or(0, |k| k.id) }),
            _ => {
                self.open_add_key(window, cx);
                if kind == "gen" {
                    self.set_key_mode(true, window, cx);
                }
            }
        }
    }

    fn open_add_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let area = |ph: &'static str, rows: usize, window: &mut Window, cx: &mut App| cx.new(|cx| TextareaState::new(window, cx).placeholder(ph).rows(rows));
        let d = KeyDialog {
            generate: false,
            name: cx.new(|cx| InputState::new(window, cx).placeholder("名称,例如 生产环境私钥")),
            private: area("私钥(粘贴 OpenSSH 私钥内容)", 5, window, cx),
            public: area("公钥(可选,留空则从私钥推导)", 3, window, cx),
            error: None,
            tried: false,
        };
        let focus = d.name.focus_handle(cx);
        window.focus(&focus, cx);
        self.key_modal = Some(KeyModal::Add(d));
        cx.notify();
    }

    fn set_key_mode(&mut self, generate: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(KeyModal::Add(d)) = &mut self.key_modal {
            d.generate = generate;
            d.error = None;
            let focus = d.name.focus_handle(cx);
            window.focus(&focus, cx);
            cx.notify();
        }
    }

    fn browse_key_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("选择私钥".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| this.load_key_file(&path, window, cx));
        })
        .detach();
    }

    /// 把私钥文件(和同名 `.pub`)读进表单;名称留空时用文件名。
    fn load_key_file(&mut self, path: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(KeyModal::Add(d)) = &self.key_modal else { return };
        let pem = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return self.key_error(format!("读取 {} 失败:{e}", path.display()), cx),
        };
        let public = std::fs::read_to_string(format!("{}.pub", path.display())).unwrap_or_default();
        let Some(KeyModal::Add(d)) = &mut self.key_modal else { return };
        d.error = None;
        d.private.update(cx, |s, cx| s.set_value(pem.trim_end().to_string(), window, cx));
        d.public.update(cx, |s, cx| s.set_value(public.trim().to_string(), window, cx));
        if val(&d.name, cx).is_empty() {
            if let Some(stem) = path.file_name().and_then(|n| n.to_str()) {
                d.name.update(cx, |s, cx| s.set_value(stem.to_string(), window, cx));
            }
        }
        cx.notify();
    }

    fn key_error(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        if let Some(KeyModal::Add(d)) = &mut self.key_modal {
            d.error = Some(msg.into());
        }
        cx.notify();
    }

    /// 导入或生成,写库。
    fn submit_key(&mut self, cx: &mut Context<Self>) {
        let Some(KeyModal::Add(d)) = &mut self.key_modal else { return };
        d.tried = true;
        let (generate, name) = (d.generate, val(&d.name, cx));
        let (private, public) = (d.private.read(cx).value().to_string(), d.public.read(cx).value().to_string());
        if name.is_empty() || (!generate && private.trim().is_empty()) {
            cx.notify();
            return;
        }
        if self.store.is_none() {
            return self.key_error("演示模式不保存密钥", cx);
        }
        // 解析 / 生成(不碰库,失败不留痕迹)
        let (pem, parsed) = if generate {
            match generate_ed25519(&name) {
                Ok(r) => r,
                Err(e) => return self.key_error(e, cx),
            }
        } else {
            let pem = Zeroizing::new(format!("{}\n", private.trim()));
            let parsed = match inspect(&pem) {
                Ok(p) => p,
                Err(e) => return self.key_error(e, cx),
            };
            if !public.trim().is_empty() {
                if let Err(e) = check_public(&parsed, &public) {
                    return self.key_error(e, cx);
                }
            }
            (pem, parsed)
        };
        let source = if generate { "生成" } else { "导入" };
        if let Err(e) = self.ensure_vault() {
            return self.key_error(e, cx);
        }
        let vault = self.vault.as_ref().expect("ensure_vault 成功");
        let new = store::NewKey { name: &name, algo: &parsed.algo, public_key: &parsed.public_key, fingerprint: &parsed.fingerprint, has_passphrase: parsed.encrypted, source };
        let res = self.store.as_mut().expect("上面检查过").add_key(&new, |id| {
            let (n, ct) = vault.seal(pem.as_bytes(), &key_aad(id, store::KEY_SLOT_PRIVATE))?;
            Ok(vec![(store::KEY_SLOT_PRIVATE.to_string(), n, ct)])
        });
        match res {
            Ok(_) => {
                crate::logs::push(crate::logs::Level::Info, "钥匙串", format!("已{}密钥 {name}({})", if generate { "生成" } else { "导入" }, parsed.fingerprint));
                self.reload_keys();
                self.key_modal = None;
                cx.notify();
            }
            Err(e) => {
                crate::logs::push(crate::logs::Level::Error, "钥匙串", format!("保存密钥失败: {e}"));
                self.key_error(format!("保存失败:{e}"), cx);
            }
        }
    }

    fn copy_public_key(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(k) = self.keys.get(i) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(k.public_key.clone()));
        self.key_copied = Some(k.id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
            let _ = this.update(cx, |this, cx| {
                this.key_copied = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn open_rename_key(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(k) = self.keys.get(i) else { return };
        let (id, name) = (k.id, k.name.clone());
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("名称").default_value(name));
        let focus = input.focus_handle(cx);
        window.focus(&focus, cx);
        self.key_modal = Some(KeyModal::Rename { id, input });
        cx.notify();
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(KeyModal::Rename { id, input }) = &self.key_modal else { return };
        let (id, name) = (*id, val(input, cx));
        if name.is_empty() {
            return;
        }
        if let Some(Err(e)) = self.store.as_ref().map(|s| s.rename_key(id, &name)) {
            crate::logs::push(crate::logs::Level::Error, "钥匙串", format!("重命名失败: {e}"));
        }
        self.reload_keys();
        self.key_modal = None;
        cx.notify();
    }

    fn confirm_delete_key(&mut self, cx: &mut Context<Self>) {
        let Some(KeyModal::Delete { id }) = &self.key_modal else { return };
        let id = *id;
        let name = self.keys.iter().find(|k| k.id == id).map(|k| k.name.clone()).unwrap_or_default();
        match self.store.as_ref().map(|s| s.delete_key(id)) {
            Some(Ok(())) => crate::logs::push(crate::logs::Level::Info, "钥匙串", format!("已删除密钥 {name}")),
            Some(Err(e)) => crate::logs::push(crate::logs::Level::Error, "钥匙串", format!("删除密钥失败: {e}")),
            None => {}
        }
        self.reload_keys();
        self.key_modal = None;
        cx.notify();
    }

    // ---- 钥匙串视图 ----

    pub(crate) fn keychain_view(&self, cx: &mut Context<Self>) -> Div {
        let add = crate::ui::button("key-add", IconName::Plus, "添加密钥").on_click(cx.listener(|this, _, window, cx| this.open_add_key(window, cx)));
        let body = if self.keys.is_empty() {
            empty_state(IconName::KeyRound, "还没有密钥", "导入已有的私钥,或生成一对新的;节点可以直接选用").into_any_element()
        } else {
            let rows: Vec<_> = self
                .keys
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let copied = self.key_copied == Some(k.id);
                    let act = |id: &'static str, ic: IconName, color: Rgba, tip: &'static str| {
                        div()
                            .id((id, i))
                            .size(px(26.0))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(t::RADIUS_SM))
                            .cursor_pointer()
                            .hover(|s| s.bg(c::hover()))
                            .child(icon(ic, 14.0, color))
                            .tooltip(move |_, cx| cx.new(|_| crate::files::Tip(tip.into())).into())
                    };
                    crate::pages::dockerpages::row(("key", i))
                        .h(px(52.0))
                        .group(format!("key-{i}"))
                        .child(
                            div()
                                .size(px(32.0))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(t::RADIUS_MD))
                                .border_1()
                                .border_color(c::accent().opacity(0.28))
                                .bg(c::accent_soft())
                                .child(icon(IconName::KeyRound, 16.0, c::accent())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(k.name.clone()))
                                .child(div().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(k.fingerprint.clone())),
                        )
                        .child(div().w(px(110.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(8.0)).child(tag(k.algo.trim_start_matches("ssh-").to_string())).when(k.has_passphrase, |d| d.child(icon(IconName::Lock, 13.0, c::muted()))))
                        .child(
                            div()
                                .w(px(84.0))
                                .flex_shrink_0()
                                .text_size(px(t::TYPE_CAPTION))
                                .text_color(c::muted())
                                .child(if k.used_by == 0 { "未使用".to_string() } else { format!("{} 个节点", k.used_by) }),
                        )
                        .child(
                            div()
                                .w(px(84.0))
                                .flex()
                                .flex_row()
                                .justify_end()
                                .flex_shrink_0()
                                .when(!copied, |d| d.opacity(0.0).group_hover(format!("key-{i}"), |s| s.opacity(1.0)))
                                .child(
                                    act("key-copy", if copied { IconName::Check } else { IconName::Copy }, if copied { c::online() } else { c::muted() }, if copied { "已复制" } else { "复制公钥" })
                                        .on_click(cx.listener(move |this, _, _, cx| this.copy_public_key(i, cx))),
                                )
                                .child(act("key-rename", IconName::Pencil, c::muted(), "重命名").on_click(cx.listener(move |this, _, window, cx| this.open_rename_key(i, window, cx))))
                                .child(act("key-del", IconName::Trash, c::danger(), "删除").on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(k) = this.keys.get(i) {
                                        this.key_modal = Some(KeyModal::Delete { id: k.id });
                                        cx.notify();
                                    }
                                }))),
                        )
                })
                .collect();
            use crate::pages::dockerpages::head;
            let header = div().child(div().w(px(32.0)).flex_shrink_0()).child(head("名称 / 指纹").flex_1()).child(head("类型").w(px(110.0)).flex_shrink_0()).child(head("使用").w(px(84.0)).flex_shrink_0()).child(div().w(px(84.0)).flex_shrink_0());
            Self::list_table("keychain-scroll", header, rows).into_any_element()
        };
        Self::side_view("钥匙串", "管理 SSH 密钥对。私钥加密后保存在本机,节点可以直接选用", Some(add), body)
    }

    // ---- 弹窗 ----

    pub(crate) fn key_modal_view(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let m = self.key_modal.as_ref()?;
        let focus = |s: &Entity<InputState>, cx: &Context<Self>| s.focus_handle(cx).is_focused(window);
        let primary = |id: &'static str, label: &'static str, enabled: bool| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .h(px(34.0))
                .px(px(16.0))
                .rounded(px(t::RADIUS_MD))
                .text_size(px(t::TYPE_CONTROL))
                .font_weight(FontWeight::SEMIBOLD)
                .when(enabled, |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
                .when(!enabled, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
                .child(label)
        };
        let title = |text: &'static str, sub: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .px(px(24.0))
                .pt(px(24.0))
                .pb(px(14.0))
                .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child(text))
                .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(sub))
        };
        let footer = |status: Option<SharedString>, buttons: Vec<AnyElement>| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .px(px(24.0))
                .py(px(14.0))
                .border_t_1()
                .border_color(c::line())
                .when_some(status, |d, e| {
                    d.child(div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(5.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::danger()).child(icon(IconName::CircleX, 13.0, c::danger())).child(e))
                })
                .child(div().flex_1())
                .children(buttons)
        };
        let cancel = ctl_btn("key-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
            this.key_modal = None;
            cx.notify();
        }));

        let (key_id, children): (&'static str, Vec<AnyElement>) = match m {
            KeyModal::Add(d) => {
                let tab = |id: &'static str, label: &'static str, on: bool| {
                    div()
                        .id(id)
                        .h(px(30.0))
                        .px(px(14.0))
                        .flex()
                        .items_center()
                        .rounded(px(t::RADIUS_MD))
                        .cursor_pointer()
                        .text_size(px(t::TYPE_CONTROL))
                        .when(on, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                        .when(!on, |d| d.text_color(c::muted()).hover(|s| s.text_color(c::secondary())))
                        .child(label)
                };
                let name_err = d.tried && val(&d.name, cx).is_empty();
                let priv_err = d.tried && !d.generate && d.private.read(cx).value().trim().is_empty();
                // 多行框:无描边的输入放进和 field 同款的圆角底(多行所以不用固定行高)
                let area = |ts: &Entity<TextareaState>, err: bool, h: f32, cx: &Context<Self>| {
                    let focused = ts.focus_handle(cx).is_focused(window);
                    div()
                        .px(px(10.0))
                        .py(px(6.0))
                        .rounded(px(t::RADIUS_MD))
                        .bg(c::field())
                        .border_1()
                        .border_color(if err { c::danger() } else if focused { c::accent() } else { c::field_border() })
                        .h(px(h))
                        .overflow_hidden()
                        .text_color(c::text())
                        .text_size(px(t::TYPE_CONTROL))
                        .font_family(MONO)
                        .child(Textarea::new(ts).appearance(false).h_full())
                };
                let mut rows: Vec<AnyElement> = vec![field(IconName::Tag, focus(&d.name, cx), name_err, input_cell(&d.name, false)).into_any_element()];
                if !d.generate {
                    rows.push(area(&d.private, priv_err, KEY_AREA_H, cx).into_any_element());
                    rows.push(area(&d.public, false, KEY_AREA_H, cx).into_any_element());
                    // 拖入或点击选择私钥文件
                    rows.push(
                        div()
                            .id("key-drop")
                            .h(px(KEY_AREA_H))
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(4.0))
                            .rounded(px(t::RADIUS_MD))
                            .border_1()
                            .border_dashed()
                            .border_color(c::field_border())
                            .cursor_pointer()
                            .hover(|s| s.bg(c::hover()))
                            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(c::accent_soft()).border_color(c::accent()))
                            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                                if let Some(p) = paths.paths().first() {
                                    this.load_key_file(p, window, cx);
                                }
                            }))
                            .on_click(cx.listener(|this, _, window, cx| this.browse_key_file(window, cx)))
                            .child(icon(IconName::FileKey, 18.0, c::muted()))
                            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("拖入私钥文件,或点击选择文件导入"))
                            .into_any_element(),
                    );
                }
                                let body = div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .px(px(24.0))
                    .pb(px(20.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .pb(px(4.0))
                            .child(tab("key-tab-import", "导入私钥", !d.generate).on_click(cx.listener(|this, _, window, cx| this.set_key_mode(false, window, cx))))
                            .child(tab("key-tab-gen", "生成新密钥", d.generate).on_click(cx.listener(|this, _, window, cx| this.set_key_mode(true, window, cx)))),
                    )
                    .children(rows);
                let submit = primary("key-submit", if d.generate { "生成" } else { "导入" }, true).on_click(cx.listener(|this, _, _, cx| this.submit_key(cx)));
                ("key-add-dialog", vec![title("添加密钥", "钥匙串里的密钥可以被多个节点共用").into_any_element(), body.into_any_element(), footer(d.error.clone(), vec![cancel.into_any_element(), submit.into_any_element()]).into_any_element()])
            }
            KeyModal::Rename { input, .. } => {
                let body = div().px(px(24.0)).pb(px(20.0)).child(field(IconName::Tag, focus(input, cx), false, input_cell(input, false)));
                let submit = primary("key-rename-ok", "保存", !val(input, cx).is_empty()).on_click(cx.listener(|this, _, _, cx| this.submit_rename(cx)));
                ("key-rename-dialog", vec![title("重命名密钥", "只改名称,不影响已经使用它的节点").into_any_element(), body.into_any_element(), footer(None, vec![cancel.into_any_element(), submit.into_any_element()]).into_any_element()])
            }
            KeyModal::Delete { id } => {
                let k = self.keys.iter().find(|k| k.id == *id);
                let name = k.map(|k| k.name.clone()).unwrap_or_default();
                let used = k.map_or(0, |k| k.used_by);
                let mut points: Vec<(IconName, Rgba, SharedString)> = Vec::new();
                if used > 0 {
                    points.push((IconName::Server, c::warn(), format!("{used} 个节点正在使用,删除后需要重新选择密钥才能连接").into()));
                }
                points.push((IconName::KeyRound, c::danger(), "私钥会从本机删除".into()));
                let spec = crate::confirm::Confirm {
                    ids: ["key-del-cancel", "key-del-ok"],
                    title: "删除密钥",
                    tile: Self::icon_tile(IconName::KeyRound, c::accent(), c::accent_soft()).into_any_element(),
                    name: name.clone().into(),
                    detail: k.map(|k| k.fingerprint.clone()).unwrap_or_default().into(),
                    points,
                    ok_label: "删除密钥",
                    compact: false,
                    kind: crate::confirm::ConfirmKind::Danger,
                    extra: None,
                };
                return Some(self.delete_confirm("key-del-dialog", spec, |this| this.key_modal = None, |this, cx| this.confirm_delete_key(cx), cx));
            }
        };
        Some(crate::ui::modal_bg(
            key_id,
            440.0,
            c::dialog(),
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    this.key_modal = None;
                    cx.notify();
                    cx.stop_propagation();
                }
                "enter" => {
                    match this.key_modal {
                        Some(KeyModal::Add(_)) => this.submit_key(cx),
                        Some(KeyModal::Rename { .. }) => this.submit_rename(cx),
                        _ => {}
                    }
                    cx.stop_propagation();
                }
                _ => {}
            }),
            children,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{check_public, generate_ed25519, inspect};
    use russh::keys::{PrivateKey, ssh_key::LineEnding};

    #[test]
    fn generate_then_inspect_roundtrip() {
        let (pem, p) = generate_ed25519("t").unwrap();
        assert_eq!(p.algo, "ssh-ed25519");
        assert!(p.public_key.starts_with("ssh-ed25519 "));
        assert!(!p.public_key.contains(" t"), "公钥不带注释");
        assert!(p.fingerprint.starts_with("SHA256:"));
        assert!(!p.encrypted);
        assert_eq!(inspect(&pem).unwrap(), p);
        // 生成的私钥能直接用于连接
        assert!(russh::keys::decode_secret_key(&pem, None).is_ok());
    }

    #[test]
    fn encrypted_key_is_detected() {
        let (pem, p) = generate_ed25519("t").unwrap();
        let key = PrivateKey::from_openssh(pem.as_bytes()).unwrap();
        let enc = key.encrypt(&mut rand::rng(), b"pw").unwrap().to_openssh(LineEnding::LF).unwrap();
        let got = inspect(&enc).unwrap();
        assert!(got.encrypted);
        assert_eq!(got.fingerprint, p.fingerprint);
    }

    #[test]
    fn public_key_must_match() {
        let (_, a) = generate_ed25519("a").unwrap();
        let (_, b) = generate_ed25519("b").unwrap();
        assert!(check_public(&a, &a.public_key).is_ok());
        assert!(check_public(&a, &format!("{} 带注释", a.public_key)).is_ok());
        assert!(check_public(&a, &b.public_key).is_err());
        assert!(check_public(&a, "garbage").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(inspect("not a key").is_err());
    }
}
