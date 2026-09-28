//! Tab that shows a SQL object's details (table/column/function markdown).
//!
//! Opened by Cmd/Ctrl+click on an identifier and by the editor context menu's
//! 「查看对象详情」. One tab per object: re-resolving the same object activates
//! the existing tab instead of stacking duplicates.

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Window, div,
};
use one_assets::IconName;
use one_core::tab_container::{
    GlobalTabContainer, TabContainer, TabContent, TabContentEvent, TabItem,
};
use rust_i18n::t;

use crate::sql_editor_hover::{SqlObjectDetails, SqlObjectDetailsKind};

/// Tab id prefix; the object slug keeps one tab per object.
const TAB_ID_PREFIX: &str = "sql-object-details:";

/// `from` value for details tabs. Deliberately not a connection id: details are
/// a snapshot of the editor's schema, so they must not be swept away together
/// with a connection's tabs.
const TAB_SOURCE: &str = "sql-object-details";

/// Tab id for an object slug produced by
/// [`crate::sql_editor_hover::resolve_object_details`].
pub fn object_details_tab_id(object_id: &str) -> String {
    format!("{TAB_ID_PREFIX}{object_id}")
}

/// Opens the details tab for `details`, or re-activates it when it is already
/// open.
///
/// `host` is the tab container the requesting view lives in: details are a
/// sibling of the SQL editor tab, so they belong in the database tab's inner
/// container rather than the window-level tab bar. Embedded editors (`host` is
/// `None`) fall back to the window container.
///
/// The tab is added on the next effect cycle rather than on the spot: the
/// Cmd/Ctrl+click entry point runs inside the editor input's own `update`
/// (`InputBaseState::go_to_definition`), and activating a tab first deactivates
/// the one that is active - the SQL editor tab's `on_deactivate` writes back
/// into that same input state, which would be a double lease. Deferring lets
/// every entity leave the stack first.
///
/// Returns `false` when neither container is available (an editor outside any
/// window container).
pub fn open_object_details_tab(
    details: &SqlObjectDetails,
    host: Option<Entity<TabContainer>>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let container = host.or_else(|| {
        cx.try_global::<GlobalTabContainer>()
            .map(|global| global.primary_pane())
    });
    let Some(container) = container else {
        return false;
    };
    let details = details.clone();
    window.defer(cx, move |window, cx| {
        add_object_details_tab(&details, &container, window, cx);
    });
    true
}

fn add_object_details_tab(
    details: &SqlObjectDetails,
    container: &Entity<TabContainer>,
    window: &mut Window,
    cx: &mut App,
) {
    let tab_id = object_details_tab_id(&details.id);
    let markdown: SharedString = details.markdown.clone().into();
    let title: SharedString = t!(
        "Query.object_details_title",
        object = details.label.as_str()
    )
    .to_string()
    .into();
    let icon = object_details_icon(details.kind);
    container.update(cx, |container, cx| {
        container.activate_or_add_tab_lazy(
            tab_id.clone(),
            move |window, cx| {
                let tab = cx.new(|cx| SqlObjectDetailsTab::new(markdown, title, icon, window, cx));
                TabItem::new(tab_id, TAB_SOURCE, tab)
            },
            window,
            cx,
        );
    });
}

fn object_details_icon(kind: SqlObjectDetailsKind) -> IconName {
    match kind {
        SqlObjectDetailsKind::Table => IconName::Table,
        SqlObjectDetailsKind::Column => IconName::Column,
        SqlObjectDetailsKind::Function => IconName::FileText,
    }
}

/// The details body itself: read-only markdown, selectable and scrollable.
struct SqlObjectDetailsTab {
    markdown: SharedString,
    title: SharedString,
    icon: IconName,
    focus_handle: FocusHandle,
}

impl SqlObjectDetailsTab {
    fn new(
        markdown: SharedString,
        title: SharedString,
        icon: IconName,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            markdown,
            title,
            icon,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Focusable for SqlObjectDetailsTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<TabContentEvent> for SqlObjectDetailsTab {}

impl Render for SqlObjectDetailsTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sql-object-details")
            .size_full()
            .p_3()
            .overflow_y_scroll()
            .child(
                gpui_base::TextView::markdown("sql-object-details-markdown", self.markdown.clone())
                    .selectable(true),
            )
    }
}

impl TabContent for SqlObjectDetailsTab {
    fn content_key(&self) -> &'static str {
        "SqlObjectDetails"
    }

    fn title(&self, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn icon(&self, _cx: &App) -> Option<gpui_component::Icon> {
        Some(self.icon.color())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_ids_are_prefixed_so_details_tabs_never_collide_with_other_tabs() {
        assert_eq!(
            object_details_tab_id("app.public:table:users"),
            "sql-object-details:app.public:table:users"
        );
    }

    #[test]
    fn object_kinds_get_distinct_icons() {
        let icons = [
            object_details_icon(SqlObjectDetailsKind::Table),
            object_details_icon(SqlObjectDetailsKind::Column),
            object_details_icon(SqlObjectDetailsKind::Function),
        ];

        for (index, icon) in icons.iter().enumerate() {
            assert!(
                !icons[..index].contains(icon),
                "each object kind needs its own icon"
            );
        }
    }
}
