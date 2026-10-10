//! Right-dock layout (#88): panel groups with fixed heights, like Photoshop's dock columns.
//!
//! A group's height never follows its content: content taller than the group scrolls inside
//! it. The last expanded group (Layers by default) fills what the others leave. Drag the gap
//! between two groups to resize them, double-click a tab (or use the panel menu) to collapse a
//! group to its tab strip, and drag a tab strip to move the group up or down the column
//! (unless Window › Workspace › Lock Workspace is on). Drag a tab strip out of the column (or
//! choose Float Group) to float it as a window; drag the window title back onto the dock to
//! dock it again. Lock Workspace blocks float, dock and reorder.
//!
//! The layout is [`DockLayout`] in `UiState::dock` (serialisable, drivable with `ui.set`), saved
//! with Window › Workspace › New Workspace…, reset by Reset Workspace, and remembered across
//! launches in the preferences (`panelLayout`) while Remember Workspace Changes is on.

use std::collections::BTreeMap;

use egui::{Rect, Sense, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::state::{DockTabs, Panels};
use crate::theme::Tokens;
use crate::widgets;

/// A dock panel group (one tab strip).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Group {
    /// Color | Swatches | Gradients | Patterns.
    Color,
    /// Properties | Adjustments.
    Properties,
    /// Character | Paragraph (not in the default workspace; Window › Character opens it, #150).
    Character,
    /// Navigator | Histogram | Info.
    Navigator,
    /// History | Actions | Layer Comps.
    History,
    /// Layers | Channels | Paths.
    Layers,
}

impl Group {
    /// Photoshop Essentials order, top to bottom.
    pub const ALL: [Group; 6] = [Group::Color, Group::Properties, Group::Character, Group::Navigator, Group::History, Group::Layers];

    pub fn key(self) -> &'static str {
        match self {
            Group::Color => "color",
            Group::Properties => "properties",
            Group::Character => "character",
            Group::Navigator => "navigator",
            Group::History => "history",
            Group::Layers => "layers",
        }
    }

    /// Height (points, tab strip included) a group gets until the user resizes it, when the
    /// column has room (see [`DockLayout::heights_for`]).
    pub fn default_height(self) -> f32 {
        match self {
            Group::Color => 190.0,
            Group::Properties => 250.0,
            Group::Character => 270.0,
            Group::Navigator => 210.0,
            Group::History => 200.0,
            Group::Layers => 320.0,
        }
    }

    /// What a group left at its default height gives way down to so the filler (Layers) keeps
    /// [`Group::preferred_fill`] in a short column (#147). Content taller than this scrolls.
    pub fn compact_height(self) -> f32 {
        match self {
            Group::Color => 130.0,
            Group::Properties => 160.0,
            Group::Character => 160.0,
            Group::Navigator => 140.0,
            Group::History => 130.0,
            Group::Layers => 200.0,
        }
    }

    /// The height the filling group asks for before default-sized groups above it get their
    /// full defaults: Layers wants room for about ten rows at 900 pt (#147).
    pub fn preferred_fill(self) -> f32 {
        match self {
            Group::Layers => 500.0,
            g => g.min_height(),
        }
    }

    /// The smallest height an expanded group can be dragged or squeezed to.
    pub fn min_height(self) -> f32 {
        match self {
            Group::Layers => 140.0,
            _ => 80.0,
        }
    }

    pub fn tabs(self, pro: bool) -> &'static [&'static str] {
        match self {
            // Photoshop Essentials: Color | Swatches | Gradients | Patterns.
            Group::Color if pro => &["Color", "Swatches", "Gradients", "Patterns"],
            Group::Color => &["Swatches", "Color", "Gradients", "Patterns"],
            Group::Properties => &["Properties", "Adjustments"],
            Group::Character => &["Character", "Paragraph"],
            Group::Navigator => &["Navigator", "Histogram", "Info"],
            Group::History => &["History", "Actions", "Layer Comps"],
            Group::Layers => &["Layers", "Channels", "Paths"],
        }
    }

    /// Tabs that lay out their own scrolling list and footer (they fill the group).
    pub fn scrolls_itself(self, tab: usize) -> bool {
        matches!((self, tab), (Group::Layers, 0) | (Group::History, 0))
    }

    fn tab_mut(self, tabs: &mut DockTabs) -> &mut usize {
        match self {
            Group::Color => &mut tabs.color,
            Group::Properties => &mut tabs.properties,
            Group::Character => &mut tabs.character,
            Group::Navigator => &mut tabs.navigator,
            Group::History => &mut tabs.history,
            Group::Layers => &mut tabs.layers,
        }
    }

    /// The group for a `panels` / `dockTabs` key ("color", "properties", …).
    pub fn from_key(key: &str) -> Option<Group> {
        Group::ALL.into_iter().find(|g| g.key() == key)
    }

    pub fn shown(self, panels: &Panels) -> bool {
        match self {
            Group::Color => panels.color,
            Group::Properties => panels.properties,
            Group::Character => panels.character,
            Group::Navigator => panels.navigator,
            Group::History => panels.history,
            Group::Layers => panels.layers,
        }
    }

    fn shown_mut(self, panels: &mut Panels) -> &mut bool {
        match self {
            Group::Color => &mut panels.color,
            Group::Properties => &mut panels.properties,
            Group::Character => &mut panels.character,
            Group::Navigator => &mut panels.navigator,
            Group::History => &mut panels.history,
            Group::Layers => &mut panels.layers,
        }
    }
}

/// Gap between groups; it is also the splitter's grab area.
pub const GAP: f32 = 6.0;
/// Upper bound on a stored height (guards against absurd values from `ui.set`).
const MAX_HEIGHT: f32 = 4000.0;
/// How far left of the dock a tab-strip drop must be before the group floats (points).
const FLOAT_OUT: f32 = 24.0;

/// A group drawn as a floating window instead of in the dock column (UI-217-5).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FloatingGroup {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Default for FloatingGroup {
    fn default() -> Self {
        Self { x: 80.0, y: 80.0, w: 280.0, h: 240.0 }
    }
}

impl FloatingGroup {
    fn sanitized(self, g: Group) -> Self {
        let x = if self.x.is_finite() { self.x } else { 80.0 };
        let y = if self.y.is_finite() { self.y } else { 80.0 };
        let w = if self.w.is_finite() { self.w.clamp(180.0, MAX_HEIGHT) } else { 280.0 };
        let h = if self.h.is_finite() { self.h.clamp(g.min_height(), MAX_HEIGHT) } else { g.default_height() };
        Self { x, y, w, h }
    }
}

/// Order, heights, collapsed state, floating groups and visibility of individual dock tabs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockLayout {
    /// Top-to-bottom order; a group missing here (say one added after the layout was saved)
    /// goes just below the nearest group that precedes it by default (or last).
    pub order: Vec<Group>,
    /// Heights the user dragged groups to (points, tab strip included). Unset = default.
    pub heights: BTreeMap<Group, f32>,
    /// Groups collapsed to their tab strip.
    pub collapsed: Vec<Group>,
    /// Groups taken out of the dock column, keyed by group, with their window rect.
    pub floating: BTreeMap<Group, FloatingGroup>,
    /// Names of tabs hidden by Close (not positions: Color/Swatches swap with the theme).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hidden_tabs: BTreeMap<Group, Vec<String>>,
}

impl DockLayout {
    /// Visible tabs, retaining their original indices for commands and panel bodies.
    pub fn visible_tabs(&self, group: Group, pro: bool) -> Vec<(usize, &'static str)> {
        let hidden = self.hidden_tabs.get(&group);
        group.tabs(pro).iter().copied().enumerate().filter(|(_, name)| !hidden.is_some_and(|xs| xs.iter().any(|x| x.as_str() == *name))).collect()
    }

    /// Hide one tab; it can be reopened through Window › Panel.
    pub fn hide_tab(&mut self, group: Group, tab: usize, pro: bool) {
        let Some(&name) = group.tabs(pro).get(tab) else { return };
        let hidden = self.hidden_tabs.entry(group).or_default();
        if !hidden.iter().any(|x| x.as_str() == name) {
            hidden.push(name.to_owned());
        }
    }

    /// Restore the tab requested by the Window menu or icon rail.
    pub fn show_tab(&mut self, group: Group, tab: usize, pro: bool) {
        let Some(&name) = group.tabs(pro).get(tab) else { return };
        if let Some(hidden) = self.hidden_tabs.get_mut(&group) {
            hidden.retain(|n| n != name);
            if hidden.is_empty() {
                self.hidden_tabs.remove(&group);
            }
        }
    }

    /// Every group once, in display order.
    pub fn order(&self) -> Vec<Group> {
        let mut out: Vec<Group> = Vec::with_capacity(Group::ALL.len());
        for g in &self.order {
            if !out.contains(g) {
                out.push(*g);
            }
        }
        // Missing groups slot in below their default predecessor, so a group new to an old
        // saved layout (Character) never lands below Layers and takes over as the filler.
        for (i, g) in Group::ALL.iter().enumerate() {
            if out.contains(g) {
                continue;
            }
            let prev = Group::ALL.iter().take(i).rev().find_map(|p| out.iter().position(|x| x == p));
            match prev {
                Some(at) => out.insert(at + 1, *g),
                None => out.push(*g),
            }
        }
        out
    }

    pub fn is_collapsed(&self, g: Group) -> bool {
        self.collapsed.contains(&g)
    }

    pub fn set_collapsed(&mut self, g: Group, on: bool) {
        self.collapsed.retain(|c| *c != g);
        if on {
            self.collapsed.push(g);
        }
    }

    /// The stored (or default) height, sanitised.
    pub fn height(&self, g: Group) -> f32 {
        match self.heights.get(&g) {
            Some(h) if h.is_finite() => h.clamp(g.min_height(), MAX_HEIGHT),
            _ => g.default_height(),
        }
    }

    /// Move `g` so it is drawn just before `before` (or last when `None`).
    pub fn move_group(&mut self, g: Group, before: Option<Group>) {
        if before == Some(g) {
            return;
        }
        let mut order = self.order();
        order.retain(|x| *x != g);
        let at = before.and_then(|b| order.iter().position(|x| *x == b)).unwrap_or(order.len());
        order.insert(at, g);
        self.order = order;
    }

    pub fn is_floating(&self, g: Group) -> bool {
        self.floating.contains_key(&g)
    }

    /// Shown groups that stay in the dock column, in display order.
    pub fn docked(&self, shown: &[Group]) -> Vec<Group> {
        self.order().into_iter().filter(|g| shown.contains(g) && !self.is_floating(*g)).collect()
    }

    /// Take `g` out of the dock column and show it as a window at `at`.
    pub fn float_group(&mut self, g: Group, at: FloatingGroup) {
        self.floating.insert(g, at.sanitized(g));
    }

    /// Put `g` back in the column just before `before` (or last when `None`).
    pub fn dock_group(&mut self, g: Group, before: Option<Group>) {
        self.floating.remove(&g);
        self.move_group(g, before);
    }

    /// Put `g` back in the column at the place it already has in display order.
    pub fn dock_in_place(&mut self, g: Group) {
        self.floating.remove(&g);
    }

    /// Lay out the `shown` groups (in display order) in a column `avail` points tall with
    /// `strip`-high tab strips. Returns each group's height. The last expanded group fills the
    /// rest. Groups the user never resized give way first, down to their compact heights, so
    /// the filler gets its preferred height (Layers: ~10 rows, #147); when the column is still
    /// too short every group gives way down to its minimum height.
    pub fn heights_for(&self, shown: &[Group], avail: f32, strip: f32) -> Vec<(Group, f32)> {
        let avail = if avail.is_finite() { avail.max(0.0) } else { 0.0 };
        let filler = shown.iter().rposition(|g| !self.is_collapsed(*g));
        let mut hs: Vec<f32> = shown
            .iter()
            .enumerate()
            .map(|(i, g)| {
                if self.is_collapsed(*g) {
                    strip
                } else if Some(i) == filler {
                    0.0
                } else {
                    self.height(*g)
                }
            })
            .collect();
        if let Some(f) = filler {
            let gaps = GAP * shown.len().saturating_sub(1) as f32;
            let min_fill = shown.get(f).map_or(0.0, |g| g.min_height());
            let pref_fill = shown.get(f).map_or(0.0, |g| g.preferred_fill());
            let used: f32 = hs.iter().sum::<f32>() + gaps;
            let mut deficit = (used + pref_fill - avail).max(0.0);
            for i in (0..f).rev() {
                if deficit <= 0.0 {
                    break;
                }
                let Some(g) = shown.get(i) else { continue };
                if self.is_collapsed(*g) || self.heights.contains_key(g) {
                    continue;
                }
                if let Some(h) = hs.get_mut(i) {
                    let give = (*h - g.compact_height()).max(0.0).min(deficit);
                    *h -= give;
                    deficit -= give;
                }
            }
            let used: f32 = hs.iter().sum::<f32>() + gaps;
            let mut deficit = (used + min_fill - avail).max(0.0);
            // Squeeze the expanded groups nearest the filler first.
            for i in (0..f).rev() {
                if deficit <= 0.0 {
                    break;
                }
                let Some(g) = shown.get(i) else { continue };
                if self.is_collapsed(*g) {
                    continue;
                }
                if let Some(h) = hs.get_mut(i) {
                    let give = (*h - g.min_height()).max(0.0).min(deficit);
                    *h -= give;
                    deficit -= give;
                }
            }
            let rest = avail - hs.iter().sum::<f32>() - gaps;
            if let Some(h) = hs.get_mut(f) {
                *h = rest.max(min_fill);
            }
        }
        shown.iter().copied().zip(hs).collect()
    }
}

/// Show `g` and expand it (Window › <panel>, the icon rail): a panel asked for is always
/// brought back, whatever state it was left in (#129).
pub fn reveal(app: &mut PhotocraftApp, g: Group) {
    *g.shown_mut(&mut app.ui.panels) = true;
    let selected = *g.tab_mut(&mut app.ui.dock_tabs);
    let pro = matches!(app.ui.theme, crate::theme::ThemeKind::Pro | crate::theme::ThemeKind::ProMedium);
    app.ui.dock.show_tab(g, selected, pro);
    app.ui.dock.set_collapsed(g, false);
}

/// Icon rail click: a hidden group is shown, a collapsed one expanded and an expanded one
/// collapsed to its tab strip. A docked group is never hidden from the rail (it used to
/// toggle visibility, so one stray click made a panel vanish: #129); `docked` is false for
/// Studio's floating Properties card, which the rail shows and hides.
pub fn rail_click(app: &mut PhotocraftApp, g: Group, docked: bool) {
    if !g.shown(&app.ui.panels) {
        reveal(app, g);
    } else if !docked {
        *g.shown_mut(&mut app.ui.panels) = false;
    } else {
        let collapse = !app.ui.dock.is_collapsed(g);
        app.ui.dock.set_collapsed(g, collapse);
    }
}

/// Per-group interactions collected while drawing, applied afterwards.
enum Action {
    ToggleCollapse(Group),
    Close(Group),
    CloseTab(Group, usize),
    Move(Group, Option<Group>),
    Float(Group, FloatingGroup),
    Dock(Group, Option<Group>),
    DockInPlace(Group),
}

/// Rects of the groups drawn last frame (screen points), for tests and automation.
pub fn last_rects(ctx: &egui::Context) -> Vec<(Group, Rect)> {
    ctx.data(|d| d.get_temp::<Vec<(Group, Rect)>>(rects_id())).unwrap_or_default()
}

fn rects_id() -> egui::Id {
    egui::Id::new("dock-group-rects")
}

/// A group's tab strip as drawn last frame (screen points), for tests and automation.
#[derive(Clone, Debug, PartialEq)]
pub struct StripRects {
    pub group: Group,
    /// `(tab index, rect)` of the tabs on the strip (the others are in the chevron menu).
    pub tabs: Vec<(usize, Rect)>,
    /// The panel menu button.
    pub menu: Rect,
    /// The » overflow button, when some tabs didn't fit.
    pub chevron: Option<Rect>,
}

/// The tab strips drawn last frame.
pub fn last_strips(ctx: &egui::Context) -> Vec<StripRects> {
    ctx.data(|d| d.get_temp::<Vec<StripRects>>(strips_id())).unwrap_or_default()
}

fn strips_id() -> egui::Id {
    egui::Id::new("dock-strip-rects")
}

fn column_id() -> egui::Id {
    egui::Id::new("dock-column-rect")
}

fn last_column(ctx: &egui::Context) -> Option<Rect> {
    ctx.data(|d| d.get_temp::<Rect>(column_id()))
}

/// Draw the `shown` groups (any order; the layout decides) filling `ui`. `body` draws one
/// group's tab content. Groups in [`DockLayout::floating`] are skipped here and drawn by
/// [`show_floating`].
pub fn show(app: &mut PhotocraftApp, ui: &mut egui::Ui, shown: &[Group], mut body: impl FnMut(&mut PhotocraftApp, &mut egui::Ui, Group, usize)) {
    let t = Tokens::get(ui.ctx());
    let strip = if t.pro { 28.0 } else { 40.0 };
    let order = app.ui.dock.docked(shown);
    let area = ui.available_rect_before_wrap();
    let heights = app.ui.dock.heights_for(&order, area.height(), strip);
    let locked = app.session.prefs().workspace_locked;
    let rects = rects_after_layout(&heights, area);
    let mut actions: Vec<Action> = Vec::new();
    let mut dragging: Option<Group> = None;
    let mut strips: Vec<StripRects> = Vec::with_capacity(rects.len());
    for (i, (g, rect)) in rects.iter().copied().enumerate() {
        let collapsed = app.ui.dock.is_collapsed(g);
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt(("dock-group", g.key())).max_rect(rect));
        child.set_clip_rect(rect.intersect(ui.clip_rect()));
        child.spacing_mut().item_spacing.y = if t.pro { 0.0 } else { 6.0 };
        let before = *g.tab_mut(&mut app.ui.dock_tabs);
        let visible = app.ui.dock.visible_tabs(g, t.pro);
        if visible.is_empty() {
            continue;
        }
        let indices: Vec<usize> = visible.iter().map(|(i, _)| *i).collect();
        let tabs: Vec<&str> = visible.iter().map(|(_, name)| *name).collect();
        let mut sel = indices.iter().position(|i| *i == before).unwrap_or(0);
        let resp = widgets::card_ex(&mut child, g.key(), &tabs, &mut sel, collapsed, |ui, shown_tab| {
            let Some(&tab) = indices.get(shown_tab) else { return };
            draw_tab_body(app, ui, g, tab, &mut body);
        });
        strips.push(StripRects {
            group: g,
            tabs: resp.tabs.iter().filter_map(|(i, r)| indices.get(*i).map(|original| (*original, *r))).collect(),
            menu: resp.menu.rect,
            chevron: resp.chevron,
        });
        // The strip uses visible indices; dockTabs and the panel bodies use original indices.
        if let Some(&picked) = indices.get(sel)
            && picked != before
        {
            *g.tab_mut(&mut app.ui.dock_tabs) = picked;
        }
        if let Some(context) = resp.tab_context {
            match context {
                crate::tab_strip::TabContextAction::Close(i) => {
                    if let Some(&tab) = indices.get(i) {
                        actions.push(Action::CloseTab(g, tab));
                    }
                }
                crate::tab_strip::TabContextAction::CloseGroup => actions.push(Action::Close(g)),
            }
        }
        if resp.strip.double_clicked() || resp.tab_double_clicked || (collapsed && resp.tab_clicked) {
            actions.push(Action::ToggleCollapse(g));
        }
        if !locked && resp.strip.dragged() {
            dragging = Some(g);
        }
        if !locked
            && resp.strip.drag_stopped()
            && let Some(p) = ui.ctx().pointer_interact_pos()
        {
            if should_float(p, area) {
                actions.push(Action::Float(g, default_float(g, &app.ui.dock, Some(area), Some(p))));
            } else {
                actions.push(Action::Move(g, drop_before(&order, &rects, g, p.y)));
            }
        }
        if dragging == Some(g) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        group_menu(app, &resp.menu, g, &tabs, sel, collapsed, locked, false, &order, &mut actions);
        // Splitter in the gap below this group: resizes it against the next expanded group.
        if i + 1 < rects.len() && !collapsed && rects.iter().skip(i + 1).any(|(n, _)| !app.ui.dock.is_collapsed(*n)) {
            let gap = Rect::from_min_size(pos2(rect.left(), rect.bottom()), vec2(rect.width(), GAP)).expand2(vec2(0.0, 2.0));
            let sresp = ui.interact(gap, ui.id().with(("dock-splitter", g.key())), Sense::drag());
            if sresp.hovered() || sresp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                ui.painter().line_segment([gap.left_center(), gap.right_center()], Stroke::new(2.0, t.accent.gamma_multiply(0.7)));
            }
            if sresp.dragged() {
                resize(&mut app.ui.dock, &heights, i, sresp.drag_delta().y);
            }
        }
    }
    // Drop indicator while a group is dragged by its tab strip (stays in the column).
    if let (Some(g), Some(p)) = (dragging, ui.ctx().pointer_interact_pos())
        && !should_float(p, area)
    {
        let before = drop_before(&order, &rects, g, p.y);
        let line_y = match before.and_then(|b| rects.iter().find(|(x, _)| *x == b)) {
            Some((_, r)) => r.top() - GAP / 2.0,
            None => rects.last().map_or(area.top(), |(_, r)| r.bottom() + GAP / 2.0),
        };
        ui.painter().line_segment([pos2(area.left(), line_y), pos2(area.right(), line_y)], Stroke::new(3.0, t.accent));
    }
    ui.ctx().data_mut(|d| {
        d.insert_temp(rects_id(), rects);
        d.insert_temp(strips_id(), strips);
        d.insert_temp(column_id(), area);
    });
    ui.advance_cursor_after_rect(area);
    apply_actions(app, actions);
}

/// Draw groups in [`DockLayout::floating`] as egui windows. `shown` is the same list as [`show`].
pub fn show_floating(app: &mut PhotocraftApp, ctx: &egui::Context, shown: &[Group], mut body: impl FnMut(&mut PhotocraftApp, &mut egui::Ui, Group, usize)) {
    let t = Tokens::get(ctx);
    let locked = app.session.prefs().workspace_locked;
    let floating: Vec<Group> = app.ui.dock.order().into_iter().filter(|g| shown.contains(g) && app.ui.dock.is_floating(*g)).collect();
    if floating.is_empty() {
        return;
    }
    let column = last_column(ctx);
    let docked_rects = last_rects(ctx);
    let docked_order: Vec<Group> = docked_rects.iter().map(|(g, _)| *g).collect();
    let mut actions: Vec<Action> = Vec::new();
    let mut strips = last_strips(ctx);
    let mut rects = docked_rects.clone();
    for g in floating {
        let collapsed = app.ui.dock.is_collapsed(g);
        let fg = app.ui.dock.floating.get(&g).copied().unwrap_or_default().sanitized(g);
        let tabs = g.tabs(t.pro);
        let title = tabs.first().copied().unwrap_or("Panel");
        let before = *g.tab_mut(&mut app.ui.dock_tabs);
        let mut sel = before;
        let mut menu: Option<egui::Response> = None;
        let inner = egui::Window::new(tl!(title))
            .id(egui::Id::new(("dock-float", g.key())))
            .resizable(!locked)
            .movable(!locked)
            .collapsible(true)
            .default_open(!collapsed)
            .pivot(egui::Align2::LEFT_TOP)
            .current_pos(pos2(fg.x, fg.y))
            .default_size(vec2(fg.w, fg.h))
            .min_size(vec2(180.0, g.min_height()))
            .show(ctx, |ui| {
                ui.set_min_size(vec2((fg.w - 8.0).max(160.0), (fg.h - 28.0).max(40.0)));
                let resp = widgets::card_ex(ui, g.key(), tabs, &mut sel, collapsed, |ui, tab| {
                    draw_tab_body(app, ui, g, tab, &mut body);
                });
                strips.push(StripRects { group: g, tabs: resp.tabs.clone(), menu: resp.menu.rect, chevron: resp.chevron });
                if resp.strip.double_clicked() || resp.tab_double_clicked || (collapsed && resp.tab_clicked) {
                    actions.push(Action::ToggleCollapse(g));
                }
                menu = Some(resp.menu);
            });
        if sel != before {
            *g.tab_mut(&mut app.ui.dock_tabs) = sel.min(tabs.len().saturating_sub(1));
        }
        if let Some(menu) = menu.as_ref() {
            group_menu(app, menu, g, tabs, sel, collapsed, locked, true, &docked_order, &mut actions);
        }
        if let Some(inner) = inner {
            let r = inner.response.rect;
            rects.push((g, r));
            if r.width().is_finite() && r.height().is_finite() && app.ui.dock.is_floating(g) {
                app.ui.dock.floating.insert(g, FloatingGroup { x: r.left(), y: r.top(), w: r.width(), h: r.height() }.sanitized(g));
            }
            if !locked
                && inner.response.drag_stopped()
                && let Some(p) = ctx.pointer_interact_pos()
                && column.is_some_and(|c| c.contains(p))
            {
                actions.push(Action::Dock(g, drop_before(&docked_order, &docked_rects, g, p.y)));
            }
        }
    }
    ctx.data_mut(|d| {
        d.insert_temp(rects_id(), rects);
        d.insert_temp(strips_id(), strips);
    });
    apply_actions(app, actions);
}

fn draw_tab_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, g: Group, tab: usize, body: &mut impl FnMut(&mut PhotocraftApp, &mut egui::Ui, Group, usize)) {
    let inner = ui.available_height().max(0.0);
    if g.scrolls_itself(tab) {
        ui.set_min_height(inner);
        body(app, ui, g, tab);
    } else {
        egui::ScrollArea::vertical()
            .id_salt(("dock-scroll", g.key(), tab))
            .max_height(inner)
            .auto_shrink([false, false])
            .show(ui, |ui| body(app, ui, g, tab));
    }
}

#[allow(clippy::too_many_arguments)]
fn group_menu(
    app: &mut PhotocraftApp,
    menu: &egui::Response,
    g: Group,
    tabs: &[&str],
    sel: usize,
    collapsed: bool,
    locked: bool,
    floating: bool,
    order: &[Group],
    actions: &mut Vec<Action>,
) {
    egui::Popup::menu(menu).show(|ui| {
        ui.set_min_width(170.0);
        if tabs.get(sel) == Some(&"Layers") {
            crate::layer_row_ui::panel_menu(app, ui);
            ui.separator();
        }
        if tabs.get(sel) == Some(&"Swatches") {
            crate::swatches_ui::panel_menu(app, ui);
            ui.separator();
        }
        if ui.button(if collapsed { tl!("Expand Panel Group") } else { tl!("Collapse Panel Group") }).clicked() {
            actions.push(Action::ToggleCollapse(g));
            ui.close();
        }
        let pos = order.iter().position(|x| *x == g).unwrap_or(0);
        if ui.add_enabled(!locked && !floating && pos > 0, egui::Button::new(tl!("Move Group Up"))).clicked() {
            actions.push(Action::Move(g, order.get(pos.saturating_sub(1)).copied()));
            ui.close();
        }
        if ui.add_enabled(!locked && !floating && pos + 1 < order.len(), egui::Button::new(tl!("Move Group Down"))).clicked() {
            actions.push(Action::Move(g, order.get(pos + 2).copied()));
            ui.close();
        }
        if floating {
            if ui.add_enabled(!locked, egui::Button::new(tl!("Dock Group"))).clicked() {
                actions.push(Action::DockInPlace(g));
                ui.close();
            }
        } else if ui.add_enabled(!locked, egui::Button::new(tl!("Float Group"))).clicked() {
            let at = default_float(g, &app.ui.dock, last_column(ui.ctx()), ui.ctx().pointer_interact_pos());
            actions.push(Action::Float(g, at));
            ui.close();
        }
        ui.separator();
        if ui.button(tl!("Close Tab Group")).clicked() {
            actions.push(Action::Close(g));
            ui.close();
        }
    });
}

fn apply_actions(app: &mut PhotocraftApp, actions: Vec<Action>) {
    let pro = crate::theme::Tokens::for_kind(app.ui.theme).pro;
    for a in actions {
        match a {
            Action::ToggleCollapse(g) => {
                let on = !app.ui.dock.is_collapsed(g);
                app.ui.dock.set_collapsed(g, on);
            }
            Action::Close(g) => *g.shown_mut(&mut app.ui.panels) = false,
            Action::CloseTab(g, tab) => {
                app.ui.dock.hide_tab(g, tab, pro);
                let remaining = app.ui.dock.visible_tabs(g, pro);
                if remaining.is_empty() {
                    *g.shown_mut(&mut app.ui.panels) = false;
                } else if *g.tab_mut(&mut app.ui.dock_tabs) == tab {
                    // Prefer the next tab, or the previous one when closing the last.
                    let next = remaining.iter().find(|(i, _)| *i > tab).or_else(|| remaining.last());
                    if let Some(&(next, _)) = next {
                        *g.tab_mut(&mut app.ui.dock_tabs) = next;
                    }
                }
            }
            Action::Move(g, before) => {
                if before != Some(g) {
                    app.ui.dock.move_group(g, before);
                }
            }
            Action::Float(g, at) => app.ui.dock.float_group(g, at),
            Action::Dock(g, before) => app.ui.dock.dock_group(g, before),
            Action::DockInPlace(g) => app.ui.dock.dock_in_place(g),
        }
    }
}

fn should_float(p: egui::Pos2, column: Rect) -> bool {
    p.x < column.left() - FLOAT_OUT || !column.contains(p)
}

fn default_float(g: Group, layout: &DockLayout, column: Option<Rect>, at: Option<egui::Pos2>) -> FloatingGroup {
    let w = column.map(|c| c.width()).filter(|w| w.is_finite() && *w >= 180.0).unwrap_or(280.0);
    let h = layout.height(g);
    let (x, y) = match (at, column) {
        (Some(p), _) => (p.x, p.y),
        (None, Some(c)) => ((c.left() - w - 16.0).max(8.0), c.top() + 24.0),
        (None, None) => (80.0, 80.0),
    };
    FloatingGroup { x, y, w, h }.sanitized(g)
}

fn parse_group(params: &Value) -> Result<Group, String> {
    params
        .get("group")
        .and_then(Value::as_str)
        .and_then(Group::from_key)
        .ok_or_else(|| "pass `group` (color, properties, character, navigator, history, layers)".into())
}

fn floating_from_params(params: &Value, g: Group, layout: &DockLayout) -> FloatingGroup {
    let base = default_float(g, layout, None, None);
    let num = |k, fallback| params.get(k).and_then(Value::as_f64).map(|n| n as f32).filter(|n| n.is_finite()).unwrap_or(fallback);
    FloatingGroup { x: num("x", base.x), y: num("y", base.y), w: num("w", base.w), h: num("h", base.h) }.sanitized(g)
}

/// `window.floatPanel` `{group}` (optional `x`,`y`,`w`,`h`).
pub fn float_command(app: &mut PhotocraftApp, params: &Value) -> Result<Value, String> {
    if app.session.prefs().workspace_locked {
        return Err("workspace is locked".into());
    }
    let g = parse_group(params)?;
    let at = floating_from_params(params, g, &app.ui.dock);
    app.ui.dock.float_group(g, at);
    Ok(json!({"group": g.key(), "floating": true, "x": at.x, "y": at.y, "w": at.w, "h": at.h}))
}

/// `window.dockPanel` `{group}`; omit `before` to restore its place, `before: null` to put it last.
pub fn dock_command(app: &mut PhotocraftApp, params: &Value) -> Result<Value, String> {
    if app.session.prefs().workspace_locked {
        return Err("workspace is locked".into());
    }
    let g = parse_group(params)?;
    match params.get("before") {
        None => app.ui.dock.dock_in_place(g),
        Some(Value::Null) => app.ui.dock.dock_group(g, None),
        Some(v) => app.ui.dock.dock_group(g, v.as_str().and_then(Group::from_key)),
    }
    Ok(json!({"group": g.key(), "floating": false}))
}

fn rects_after_layout(heights: &[(Group, f32)], area: Rect) -> Vec<(Group, Rect)> {
    let mut y = area.top();
    heights
        .iter()
        .map(|(g, h)| {
            let r = Rect::from_min_size(pos2(area.left(), y), vec2(area.width(), h.max(0.0)));
            y = r.bottom() + GAP;
            (*g, r)
        })
        .collect()
}

/// The group the dragged one lands before when released at `y` (`None` = last).
fn drop_before(order: &[Group], rects: &[(Group, Rect)], dragged: Group, y: f32) -> Option<Group> {
    let hit = rects.iter().find(|(_, r)| y < r.center().y).map(|(g, _)| *g);
    match hit {
        // Dropping onto itself, or just below itself, keeps the place.
        Some(g) if g == dragged => Some(dragged),
        Some(g) => {
            let after_self = order.iter().position(|x| *x == dragged).zip(order.iter().position(|x| *x == g)).is_some_and(|(a, b)| b == a + 1);
            if after_self { Some(dragged) } else { Some(g) }
        }
        None => None,
    }
}

/// Splitter `i` (below group `i`) dragged by `dy`: group `i` grows or shrinks against the next
/// expanded group (or the filler, which absorbs the difference).
fn resize(layout: &mut DockLayout, heights: &[(Group, f32)], i: usize, dy: f32) {
    if !dy.is_finite() || dy == 0.0 {
        return;
    }
    let Some(&(g, h)) = heights.get(i) else { return };
    let filler = heights.iter().rposition(|(g, _)| !layout.is_collapsed(*g));
    let Some(j) = heights.iter().enumerate().skip(i + 1).find(|(_, (n, _))| !layout.is_collapsed(*n)).map(|(j, _)| j) else { return };
    let Some(&(n, nh)) = heights.get(j) else { return };
    // The first drag pins the other groups at the heights they show, so groups still at their
    // defaults (which give way to the filler) don't shift while this one is resized.
    for (k, (o, oh)) in heights.iter().enumerate() {
        if Some(k) != filler && !layout.is_collapsed(*o) {
            layout.heights.entry(*o).or_insert(*oh);
        }
    }
    let new_h = (h + dy).clamp(g.min_height(), (h + nh - n.min_height()).max(g.min_height()));
    layout.heights.insert(g, new_h);
    if Some(j) != filler {
        layout.heights.insert(n, (nh - (new_h - h)).max(n.min_height()));
    }
}

/// What `prefs.panelLayout` holds: the live layout, open panels, dock width and timeline.
fn snapshot(app: &PhotocraftApp, ctx: &egui::Context) -> Value {
    let mut v = json!({"workspace": app.ui.workspace, "panels": app.ui.panels, "dockTabs": app.ui.dock_tabs, "dock": app.ui.dock, "timelineOpen": app.ui.timeline.open});
    if let Some(w) = egui::containers::panel::PanelState::load(ctx, egui::Id::new("dock")).map(|p| p.size().x).filter(|w| w.is_finite()) {
        v["dockWidth"] = json!(w);
    }
    v
}

/// Remember the layout in the preferences once the user lets go of the mouse (Workspace ›
/// Remember Workspace Changes). Cheap: a small JSON compare per frame.
pub fn persist(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if !app.session.prefs().workspace.remember_workspace_changes || ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    let now = snapshot(app, ctx);
    if app.session.prefs().panel_layout != now {
        app.session.prefs.edit(|p| p.panel_layout = now);
    }
}

/// Restore the remembered layout at launch. Unreadable parts keep their defaults.
pub fn restore(app: &mut PhotocraftApp) {
    if !app.session.prefs().workspace.remember_workspace_changes {
        return;
    }
    let saved = app.session.prefs().panel_layout.clone();
    apply(app, &saved);
    if let Some(ws) = saved.get("workspace").and_then(Value::as_str) {
        app.ui.workspace = ws.to_string();
    }
}

/// Apply the `panels`, `dockTabs`, `dock` and `timelineOpen` parts of a saved layout
/// (a workspace or `panelLayout`). Missing or invalid dock parts are left alone; old
/// layouts without Timeline visibility restore it closed.
pub fn apply(app: &mut PhotocraftApp, v: &Value) {
    if let Some(p) = v.get("panels").and_then(|p| serde_json::from_value(p.clone()).ok()) {
        app.ui.panels = p;
    }
    if let Some(t) = v.get("dockTabs").and_then(|t| serde_json::from_value(t.clone()).ok()) {
        app.ui.dock_tabs = t;
    }
    if let Some(d) = v.get("dock").and_then(|d| serde_json::from_value(d.clone()).ok()) {
        app.ui.dock = d;
    }
    app.ui.timeline.open = v.get("timelineOpen").and_then(Value::as_bool).unwrap_or(false);
    if !app.ui.timeline.open {
        app.ui.timeline.playing = false;
    }
    if let Some(w) = v.get("dockWidth").and_then(Value::as_f64).filter(|w| w.is_finite()) {
        app.pending_dock_width = Some(w as f32);
    }
}

#[cfg(test)]
#[path = "dock_tests.rs"]
mod tests;
