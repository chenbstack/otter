//! 数据库页:识别服务器上的数据库容器并列出;没有 Docker 时引导去容器页。

use super::super::*;
use super::dockerpages::problem_text;
use crate::dbinst::{self, DbInstance, DbKind};
use crate::docker::Problem;

pub(crate) const DB_TABS: [(&str, &[DbKind]); 3] = [("PostgreSQL", &[DbKind::Postgres]), ("MySQL", &[DbKind::Mysql, DbKind::Mariadb]), ("Redis", &[DbKind::Redis])];

impl Home {
    /// Docker 不可用时的数据库页:说明原因,给一个去「容器」页的按钮
    pub(crate) fn db_problem_page(&self, p: Problem, cx: &mut Context<Self>) -> Div {
        let (_, title, sub) = problem_text(&p);
        let (hint, go) = match p {
            Problem::NoDocker => ("数据库以容器方式运行,请先到「容器」页安装 Docker".to_string(), "去安装 Docker"),
            _ => (sub, "去容器页处理"),
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .pb(px(72.0))
            .gap(px(10.0))
            .child(icon(IconName::Database, 34.0, c::ghost()))
            .child(div().mt(px(4.0)).text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child(title))
            .child(div().max_w(px(520.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(hint))
            .child(button("db-goto-containers", IconName::Container, go).mt(px(6.0)).on_click(cx.listener(|this, _, _, cx| {
                this.page = Page::Containers;
                cx.notify();
            })))
    }

    /// 数据库页的类型页签:版式和容器页的页签一致,挂在页头下面的同一个位置
    pub(crate) fn db_tabs(&self, cx: &mut Context<Self>) -> Div {
        let all = dbinst::detect(&self.dock.snap.containers);
        let cur = self.db_tab.min(DB_TABS.len() - 1);
        let mut row = div().flex().flex_row().items_center().gap(px(4.0)).pb(px(10.0)).flex_shrink_0();
        for (i, (name, kinds)) in DB_TABS.iter().enumerate() {
            let active = cur == i;
            let n = all.iter().filter(|d| kinds.contains(&d.kind)).count();
            row = row.child(
                div()
                    .id(("dbtab", i))
                    .h(px(30.0))
                    .px(px(12.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .text_size(px(t::TYPE_CONTROL))
                    .when(active, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()))
                    .when(!active, |d| d.rounded(px(t::RADIUS_MD)).border_1().border_color(gpui_kit::transparent_black()).text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::secondary())))
                    .child(icon(IconName::Database, 14.0, if active { c::accent() } else { c::ghost() }))
                    .child(*name)
                    .when(n > 0, |d| d.child(div().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(if active { c::accent() } else { c::ghost() }).child(n.to_string())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.db_tab = i;
                        cx.notify();
                    })),
            );
        }
        row
    }

    /// 数据库页:当前类型下选中一个实例(按端口区分),直接显示这个实例里的库/用户管理
    pub(crate) fn database_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let all: Vec<DbInstance> = dbinst::detect(&self.dock.snap.containers);
        let cur = self.db_tab.min(DB_TABS.len() - 1);
        let list: Vec<DbInstance> = all.into_iter().filter(|d| DB_TABS[cur].1.contains(&d.kind)).collect();
        if list.is_empty() {
            return div().flex_1().flex().flex_col().child(empty(IconName::Database, "没有这类数据库").desc("这台服务器上没有运行这类数据库容器")).into_any_element();
        }
        self.dbm_page(list, cx)
    }
}
