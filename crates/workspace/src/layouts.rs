//! The layouts of a workspace: several arrangements of panes, of which the
//! window shows one. The panes of the other layouts stay in
//! [`Workspace::panes`] with their items, so a hidden view keeps its state
//! and an unsaved file is still found when the window closes. Only the tree
//! that arranges them is swapped.

use crate::{
    Event, Member, Pane, PaneGroup, Workspace, WorkspaceDb, WorkspaceId, pane,
    persistence::{
        SerializedAxis,
        model::{SerializedItem, SerializedPane, SerializedPaneGroup, SerializedWorkspaceLocation},
    },
    serialize_pane_handle,
};
use anyhow::Result;
use collections::HashMap;
use gpui::{
    App, AppContext as _, AsyncWindowContext, Axis, Context, Entity, EntityId, Focusable as _,
    SharedString, Task, WeakEntity, Window,
};
use project::Project;
use remote::RemoteConnectionOptions;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use util::{ResultExt as _, path_list::PathList};

const LAYOUTS_NAMESPACE: &str = "workspace_layouts";
const FIRST_LAYOUT_NAME: &str = "Main";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LayoutId(u64);

/// Where the tabs a layout opens start: the folder new terminals run in,
/// and the workspace its overview shows. A layout without one is global.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LayoutScope {
    /// The Herdr machine; `None` for this one.
    pub machine: Option<String>,
    pub folder: String,
}

pub struct Layout {
    id: LayoutId,
    name: SharedString,
    /// The group the layout is listed under. Empty: no group.
    group: SharedString,
    scope: Option<LayoutScope>,
    /// `None` for the layout the window shows: its tree is `Workspace::center`.
    parked: Option<ParkedLayout>,
}

struct ParkedLayout {
    center: PaneGroup,
    active_pane: Entity<Pane>,
    maximized_pane: Option<WeakEntity<Pane>>,
}

impl Layout {
    pub fn id(&self) -> LayoutId {
        self.id
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    pub fn group(&self) -> &SharedString {
        &self.group
    }

    pub fn scope(&self) -> Option<&LayoutScope> {
        self.scope.as_ref()
    }
}

pub enum LayoutOutline {
    Pane(Entity<Pane>),
    Split {
        axis: Axis,
        /// Each part with its share of the space.
        children: Vec<(f32, LayoutOutline)>,
    },
}

pub(crate) struct Layouts {
    layouts: Vec<Layout>,
    active: LayoutId,
    next_id: u64,
    /// Whether there is more to save than the single layout every workspace
    /// starts with. A workspace that never used layouts writes nothing.
    customized: bool,
}

impl Layouts {
    pub(crate) fn new() -> Self {
        Self {
            layouts: vec![Layout {
                id: LayoutId(1),
                name: FIRST_LAYOUT_NAME.into(),
                group: SharedString::default(),
                scope: None,
                parked: None,
            }],
            active: LayoutId(1),
            next_id: 2,
            customized: false,
        }
    }

    fn parked(&self, id: LayoutId) -> Option<&ParkedLayout> {
        self.layouts
            .iter()
            .find(|layout| layout.id == id)?
            .parked
            .as_ref()
    }

    fn parked_layout_of(&self, pane: &Entity<Pane>) -> Option<LayoutId> {
        self.layouts.iter().find_map(|layout| {
            let parked = layout.parked.as_ref()?;
            parked
                .center
                .panes()
                .into_iter()
                .any(|parked_pane| parked_pane == pane)
                .then_some(layout.id)
        })
    }
}

impl Workspace {
    pub fn layouts(&self) -> &[Layout] {
        &self.layouts.layouts
    }

    pub fn active_layout(&self) -> LayoutId {
        self.layouts.active
    }

    pub fn layout(&self, id: LayoutId) -> Option<&Layout> {
        self.layouts.layouts.iter().find(|layout| layout.id == id)
    }

    /// The scope of the layout on screen; `None` when it is global.
    pub fn active_layout_scope(&self) -> Option<&LayoutScope> {
        self.layout(self.layouts.active)?.scope.as_ref()
    }

    /// The tree of panes of a layout.
    pub fn layout_root(&self, id: LayoutId) -> Option<&Member> {
        if id == self.layouts.active {
            return Some(&self.center.root);
        }
        Some(&self.layouts.parked(id)?.center.root)
    }

    pub fn layout_panes(&self, id: LayoutId) -> Vec<Entity<Pane>> {
        let group = if id == self.layouts.active {
            &self.center
        } else {
            match self.layouts.parked(id) {
                Some(parked) => &parked.center,
                None => return Vec::new(),
            }
        };
        group.panes().into_iter().cloned().collect()
    }

    /// The layout that shows `pane`. `None` for a pane of a dock.
    pub fn layout_for_pane(&self, pane: &Entity<Pane>) -> Option<LayoutId> {
        if let Some(layout) = self.layouts.parked_layout_of(pane) {
            return Some(layout);
        }
        self.center
            .panes()
            .into_iter()
            .any(|center_pane| center_pane == pane)
            .then_some(self.layouts.active)
    }

    pub fn layout_for_item(&self, item_id: EntityId, cx: &App) -> Option<LayoutId> {
        let pane = self
            .panes
            .iter()
            .find(|pane| pane.read(cx).items().any(|item| item.item_id() == item_id))?;
        self.layout_for_pane(pane)
    }

    /// Makes `pane` the one its layout focuses when the window shows it.
    pub(crate) fn set_parked_active_pane(&mut self, pane: &Entity<Pane>) {
        for parked in self
            .layouts
            .layouts
            .iter_mut()
            .filter_map(|layout| layout.parked.as_mut())
        {
            if parked
                .center
                .panes()
                .into_iter()
                .any(|parked_pane| parked_pane == pane)
            {
                parked.active_pane = pane.clone();
            }
        }
    }

    /// How a layout splits its space, for a client that draws it small.
    pub fn layout_outline(&self, id: LayoutId) -> Option<LayoutOutline> {
        fn outline(member: &Member) -> LayoutOutline {
            match member {
                Member::Pane(pane) => LayoutOutline::Pane(pane.clone()),
                Member::Axis(axis) => {
                    let flexes = axis.flexes.lock().clone();
                    LayoutOutline::Split {
                        axis: axis.axis,
                        children: axis
                            .members
                            .iter()
                            .enumerate()
                            .map(|(index, member)| {
                                (flexes.get(index).copied().unwrap_or(1.), outline(member))
                            })
                            .collect(),
                    }
                }
            }
        }
        self.layout_root(id).map(outline)
    }

    /// Whether `pane` belongs to a layout the window does not show.
    pub(crate) fn is_parked_pane(&self, pane: &Entity<Pane>) -> bool {
        self.layouts.parked_layout_of(pane).is_some()
    }

    /// The panes the window shows in its center.
    pub(crate) fn shown_panes(&self) -> Vec<Entity<Pane>> {
        self.center.panes().into_iter().cloned().collect()
    }

    /// Adds an empty layout and shows it.
    pub fn create_layout(
        &mut self,
        name: impl Into<SharedString>,
        group: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> LayoutId {
        let id = LayoutId(self.layouts.next_id);
        self.layouts.next_id += 1;
        self.layouts.customized = true;
        let pane = self.add_pane(window, cx);
        let mut center = PaneGroup::new(pane.clone());
        center.set_is_center(true);
        self.layouts.layouts.push(Layout {
            id,
            name: name.into(),
            group: group.into(),
            scope: None,
            parked: Some(ParkedLayout {
                center,
                active_pane: pane,
                maximized_pane: None,
            }),
        });
        self.activate_layout(id, window, cx);
        id
    }

    /// Adds a layout with a scope without showing it: the layout on screen
    /// and the focus stay where they are.
    pub fn add_hidden_layout(
        &mut self,
        name: impl Into<SharedString>,
        group: impl Into<SharedString>,
        scope: Option<LayoutScope>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> LayoutId {
        let id = LayoutId(self.layouts.next_id);
        self.layouts.next_id += 1;
        self.layouts.customized = true;
        let focused = window.focused(cx);
        // A new pane takes the focus; the hidden layout must not.
        let pane = self.add_pane(window, cx);
        match focused {
            Some(focused) => window.focus(&focused, cx),
            None => window.focus(&self.active_pane().focus_handle(cx), cx),
        }
        let mut center = PaneGroup::new(pane.clone());
        center.set_is_center(true);
        self.layouts.layouts.push(Layout {
            id,
            name: name.into(),
            group: group.into(),
            scope,
            parked: Some(ParkedLayout {
                center,
                active_pane: pane,
                maximized_pane: None,
            }),
        });
        cx.emit(Event::LayoutsChanged);
        self.serialize_workspace(window, cx);
        cx.notify();
        id
    }

    /// The pane where a layout puts a new tab: the active pane of the one on
    /// screen, the pane that was active in a hidden one.
    pub fn layout_active_pane(&self, id: LayoutId) -> Option<Entity<Pane>> {
        if id == self.layouts.active {
            return Some(self.active_pane().clone());
        }
        self.layouts
            .parked(id)
            .map(|parked| parked.active_pane.clone())
    }

    /// Shows a layout. The panes of the one it replaces keep their items.
    pub fn activate_layout(&mut self, id: LayoutId, window: &mut Window, cx: &mut Context<Self>) {
        if id == self.layouts.active {
            return;
        }
        let Some(target) = self
            .layouts
            .layouts
            .iter_mut()
            .find(|layout| layout.id == id)
            .and_then(|layout| layout.parked.take())
        else {
            return;
        };

        let previous_center = std::mem::replace(&mut self.center, target.center);
        let previous_active_pane = self
            .last_active_center_pane
            .as_ref()
            .and_then(|pane| pane.upgrade())
            .filter(|pane| {
                previous_center
                    .panes()
                    .into_iter()
                    .any(|center_pane| center_pane == pane)
            })
            .unwrap_or_else(|| previous_center.first_pane());
        let previous_maximized_pane =
            std::mem::replace(&mut self.maximized_pane, target.maximized_pane);
        let previous = self.layouts.active;
        if let Some(layout) = self
            .layouts
            .layouts
            .iter_mut()
            .find(|layout| layout.id == previous)
        {
            layout.parked = Some(ParkedLayout {
                center: previous_center,
                active_pane: previous_active_pane,
                maximized_pane: previous_maximized_pane,
            });
        }
        self.layouts.active = id;

        self.center.mark_positions(cx);
        // A zoomed pane of the center belonged to the layout that left.
        if self.zoomed_position.is_none() && self.zoomed.take().is_some() {
            cx.emit(Event::ZoomChanged);
        }
        self.set_active_pane(&target.active_pane, window, cx);
        self.status_bar.update(cx, |status_bar, cx| {
            status_bar.set_active_pane(&target.active_pane, window, cx);
        });
        window.focus(&target.active_pane.focus_handle(cx), cx);

        cx.emit(Event::LayoutsChanged);
        cx.emit(Event::ActiveItemChanged);
        self.serialize_workspace(window, cx);
        cx.notify();
    }

    /// Shows the layout after or before the one the window shows.
    pub fn activate_adjacent_layout(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.layouts.layouts.len();
        let Some(index) = self
            .layouts
            .layouts
            .iter()
            .position(|layout| layout.id == self.layouts.active)
        else {
            return;
        };
        let next = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        if let Some(layout) = self.layouts.layouts.get(next) {
            self.activate_layout(layout.id, window, cx);
        }
    }

    pub fn rename_layout(
        &mut self,
        id: LayoutId,
        name: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = name.into();
        if let Some(layout) = self
            .layouts
            .layouts
            .iter_mut()
            .find(|layout| layout.id == id)
            && layout.name != name
        {
            layout.name = name;
            self.layouts.customized = true;
            cx.emit(Event::LayoutsChanged);
            self.serialize_workspace(window, cx);
            cx.notify();
        }
    }

    pub fn set_layout_group(
        &mut self,
        id: LayoutId,
        group: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = group.into();
        if let Some(layout) = self
            .layouts
            .layouts
            .iter_mut()
            .find(|layout| layout.id == id)
            && layout.group != group
        {
            layout.group = group;
            self.layouts.customized = true;
            cx.emit(Event::LayoutsChanged);
            self.serialize_workspace(window, cx);
            cx.notify();
        }
    }

    pub fn set_layout_scope(
        &mut self,
        id: LayoutId,
        scope: Option<LayoutScope>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(layout) = self
            .layouts
            .layouts
            .iter_mut()
            .find(|layout| layout.id == id)
            && layout.scope != scope
        {
            layout.scope = scope;
            self.layouts.customized = true;
            cx.emit(Event::LayoutsChanged);
            self.serialize_workspace(window, cx);
            cx.notify();
        }
    }

    /// Closes the items of a layout, asking about unsaved ones, then removes
    /// it. The last layout of a workspace stays.
    pub fn remove_layout(
        &mut self,
        id: LayoutId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.layouts.layouts.len() < 2 || !self.layouts.layouts.iter().any(|l| l.id == id) {
            return Task::ready(Ok(()));
        }
        // A question about an unsaved file needs its file on screen.
        self.activate_layout(id, window, cx);
        let close_tasks = self
            .shown_panes()
            .into_iter()
            .map(|pane| {
                pane.update(cx, |pane, cx| {
                    pane.close_all_items(
                        &pane::CloseAllItems {
                            save_intent: None,
                            close_pinned: true,
                        },
                        window,
                        cx,
                    )
                })
            })
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |this, cx| {
            for task in close_tasks {
                task.await?;
            }
            this.update_in(cx, |this, window, cx| {
                let panes = this.layout_panes(id);
                if panes.iter().any(|pane| pane.read(cx).items_len() > 0) {
                    return;
                }
                if this.layouts.active == id {
                    let Some(index) = this.layouts.layouts.iter().position(|l| l.id == id) else {
                        return;
                    };
                    let neighbor = if index > 0 { index - 1 } else { index + 1 };
                    let Some(neighbor) = this.layouts.layouts.get(neighbor).map(|l| l.id) else {
                        return;
                    };
                    this.activate_layout(neighbor, window, cx);
                }
                this.layouts.layouts.retain(|layout| layout.id != id);
                for pane in panes {
                    this.panes.retain(|other| other != &pane);
                    this.unfollow_in_pane(&pane, window, cx);
                    this.last_leaders_by_pane.remove(&pane.downgrade());
                }
                cx.emit(Event::PaneRemoved);
                cx.emit(Event::LayoutsChanged);
                this.serialize_workspace(window, cx);
                cx.notify();
            })
        })
    }

    /// Removes a pane that lost its last item from a layout the window does
    /// not show. Returns whether `pane` was such a pane.
    pub(crate) fn remove_parked_pane(
        &mut self,
        pane: &Entity<Pane>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(parked) = self
            .layouts
            .layouts
            .iter_mut()
            .filter_map(|layout| layout.parked.as_mut())
            .find(|parked| {
                parked
                    .center
                    .panes()
                    .into_iter()
                    .any(|parked_pane| parked_pane == pane)
            })
        else {
            return false;
        };
        // The only pane of a layout stays, empty, as in the center.
        if parked.center.remove(pane, cx).log_err() == Some(true) {
            if &parked.active_pane == pane {
                parked.active_pane = parked.center.first_pane();
            }
            if parked
                .maximized_pane
                .as_ref()
                .is_some_and(|maximized| maximized.entity_id() == pane.entity_id())
            {
                parked.maximized_pane = None;
            }
            self.panes.retain(|other| other != pane);
            self.last_leaders_by_pane.remove(&pane.downgrade());
            for item in pane.read(cx).items() {
                self.panes_by_item.remove(&item.item_id());
            }
            cx.emit(Event::LayoutsChanged);
            cx.notify();
        }
        true
    }

    /// Writes every layout under one key, so that a layout and the tree the
    /// window shows cannot be saved at two different moments.
    pub(crate) fn save_layouts(
        &self,
        workspace_id: WorkspaceId,
        paths: &PathList,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<()> {
        if !self.layouts.customized {
            return Task::ready(());
        }

        fn serialize_member(
            member: &Member,
            active_pane: Option<&Entity<Pane>>,
            window: &mut Window,
            cx: &mut App,
        ) -> SavedMember {
            match member {
                Member::Axis(axis) => SavedMember::Group {
                    horizontal: axis.axis == Axis::Horizontal,
                    flexes: axis.flexes.lock().clone(),
                    children: axis
                        .members
                        .iter()
                        .map(|member| serialize_member(member, active_pane, window, cx))
                        .collect(),
                },
                Member::Pane(pane) => {
                    let serialized = serialize_pane_handle(pane, window, cx);
                    SavedMember::Pane {
                        active: active_pane == Some(pane),
                        pinned_count: serialized.pinned_count,
                        items: serialized
                            .children
                            .iter()
                            .map(|item| SavedItem {
                                kind: item.kind.to_string(),
                                item_id: item.item_id,
                                active: item.active,
                                preview: item.preview,
                            })
                            .collect(),
                    }
                }
            }
        }

        let shown_active_pane = self
            .last_active_center_pane
            .as_ref()
            .and_then(|pane| pane.upgrade());
        let saved = SavedLayouts {
            paths: saved_paths(paths),
            active: self.layouts.active,
            next_id: self.layouts.next_id,
            layouts: self
                .layouts
                .layouts
                .iter()
                .map(|layout| {
                    let (root, active_pane) = match &layout.parked {
                        Some(parked) => (&parked.center.root, Some(&parked.active_pane)),
                        None => (&self.center.root, shown_active_pane.as_ref()),
                    };
                    SavedLayout {
                        id: layout.id,
                        name: layout.name.to_string(),
                        group: layout.group.to_string(),
                        scope: layout.scope.clone(),
                        center: serialize_member(root, active_pane, window, cx),
                    }
                })
                .collect(),
        };
        let kvp = db::kvp::KeyValueStore::global(cx);
        cx.background_spawn(async move {
            let Some(json) = serde_json::to_string(&saved).log_err() else {
                return;
            };
            kvp.scoped(LAYOUTS_NAMESPACE)
                .write(layouts_key(workspace_id), json)
                .await
                .log_err();
        })
    }

    /// Loads the items of the layouts the window will not show. The caller
    /// loads the one it shows as the center of the workspace.
    pub(crate) async fn load_parked_layouts(
        saved: SavedLayouts,
        workspace: &WeakEntity<Self>,
        project: &Entity<Project>,
        workspace_id: WorkspaceId,
        cx: &mut AsyncWindowContext,
    ) -> Option<LoadedLayouts> {
        let mut loaded = LoadedLayouts {
            active: saved.active,
            next_id: saved.next_id,
            layouts: Vec::new(),
            item_ids_by_kind: HashMap::default(),
        };
        for layout in saved.layouts {
            if layout.id == saved.active {
                loaded.layouts.push(LoadedLayout {
                    id: layout.id,
                    name: layout.name,
                    group: layout.group,
                    scope: layout.scope,
                    center: None,
                });
                continue;
            }
            let deserialized = layout
                .center
                .into_pane_group()
                .deserialize(project, workspace_id, workspace.clone(), cx)
                .await;
            let center = match deserialized {
                Some((root, active_pane, items)) => {
                    cx.update(|_, cx| {
                        for item in items.into_iter().flatten() {
                            if let Some(item) = item.to_serializable_item_handle(cx) {
                                loaded
                                    .item_ids_by_kind
                                    .entry(item.serialized_item_kind())
                                    .or_default()
                                    .push(item.item_id().as_u64());
                            }
                        }
                    })
                    .log_err()?;
                    (root, active_pane)
                }
                // None of its items came back: the layout stays, empty.
                None => {
                    let pane = workspace
                        .update_in(cx, |workspace, window, cx| workspace.add_pane(window, cx))
                        .log_err()?;
                    (Member::Pane(pane), None)
                }
            };
            loaded.layouts.push(LoadedLayout {
                id: layout.id,
                name: layout.name,
                group: layout.group,
                scope: layout.scope,
                center: Some(center),
            });
        }
        Some(loaded)
    }

    pub(crate) fn restore_layouts(&mut self, loaded: LoadedLayouts, cx: &mut Context<Self>) {
        let replaced_panes = self
            .layouts
            .layouts
            .iter()
            .filter_map(|layout| layout.parked.as_ref())
            .flat_map(|parked| parked.center.panes().into_iter().cloned())
            .collect::<Vec<_>>();
        self.panes.retain(|pane| !replaced_panes.contains(pane));

        let layouts = loaded
            .layouts
            .into_iter()
            .map(|loaded| Layout {
                id: loaded.id,
                name: loaded.name.into(),
                group: loaded.group.into(),
                scope: loaded.scope,
                parked: loaded.center.map(|(root, active_pane)| {
                    let mut center = PaneGroup::with_root(root);
                    center.set_is_center(true);
                    let active_pane = active_pane.unwrap_or_else(|| center.first_pane());
                    ParkedLayout {
                        center,
                        active_pane,
                        maximized_pane: None,
                    }
                }),
            })
            .collect::<Vec<_>>();
        let highest_id = layouts.iter().map(|layout| layout.id.0).max().unwrap_or(0);
        self.layouts = Layouts {
            layouts,
            active: loaded.active,
            next_id: loaded.next_id.max(highest_id + 1),
            customized: true,
        };
        cx.emit(Event::LayoutsChanged);
    }
}

pub(crate) struct LoadedLayouts {
    active: LayoutId,
    next_id: u64,
    layouts: Vec<LoadedLayout>,
    /// The items of the loaded layouts, which the database must keep.
    pub(crate) item_ids_by_kind: HashMap<&'static str, Vec<crate::ItemId>>,
}

struct LoadedLayout {
    id: LayoutId,
    name: String,
    group: String,
    scope: Option<LayoutScope>,
    center: Option<(Member, Option<Entity<Pane>>)>,
}

/// The layouts a workspace saved, read without opening it.
pub struct SavedWorkspaceLayouts {
    pub workspace_id: WorkspaceId,
    pub paths: PathList,
    pub remote: Option<RemoteConnectionOptions>,
    pub active: LayoutId,
    pub layouts: Vec<SavedLayoutInfo>,
}

pub struct SavedLayoutInfo {
    pub id: LayoutId,
    pub name: SharedString,
    pub group: SharedString,
    pub scope: Option<LayoutScope>,
}

/// Every layout saved by any workspace, so that a window lists the layouts
/// of the workspaces it does not hold.
pub fn saved_layout_catalog(cx: &App) -> Task<Vec<SavedWorkspaceLayouts>> {
    let kvp = db::kvp::KeyValueStore::global(cx);
    let db = WorkspaceDb::global(cx);
    cx.background_spawn(async move {
        let Some(entries) = kvp.scoped(LAYOUTS_NAMESPACE).read_all().log_err() else {
            return Vec::new();
        };
        let mut catalog = Vec::new();
        for (key, json) in entries {
            let Some(id) = key.parse::<i64>().ok().map(WorkspaceId::from_i64) else {
                continue;
            };
            let Some(saved) = serde_json::from_str::<SavedLayouts>(&json).log_err() else {
                continue;
            };
            let Some(workspace) = db.workspace_for_id(id) else {
                continue;
            };
            if saved.paths != saved_paths(&workspace.paths) {
                continue;
            }
            let remote = match workspace.location {
                SerializedWorkspaceLocation::Local => None,
                SerializedWorkspaceLocation::Remote(options) => Some(options),
            };
            catalog.push(SavedWorkspaceLayouts {
                workspace_id: id,
                paths: workspace.paths,
                remote,
                active: saved.active,
                layouts: saved
                    .layouts
                    .into_iter()
                    .map(|layout| SavedLayoutInfo {
                        id: layout.id,
                        name: layout.name.into(),
                        group: layout.group.into(),
                        scope: layout.scope,
                    })
                    .collect(),
            });
        }
        catalog
    })
}

/// Makes `layout` the one a saved workspace shows when it opens next.
pub fn show_saved_layout_on_open(
    workspace_id: WorkspaceId,
    layout: LayoutId,
    cx: &App,
) -> Task<Result<()>> {
    let kvp = db::kvp::KeyValueStore::global(cx);
    cx.background_spawn(async move {
        let key = layouts_key(workspace_id);
        let Some(json) = kvp.scoped(LAYOUTS_NAMESPACE).read(&key)? else {
            return Ok(());
        };
        let mut saved = serde_json::from_str::<SavedLayouts>(&json)?;
        if saved.active == layout || !saved.layouts.iter().any(|saved| saved.id == layout) {
            return Ok(());
        }
        saved.active = layout;
        kvp.scoped(LAYOUTS_NAMESPACE)
            .write(key, serde_json::to_string(&saved)?)
            .await
    })
}

fn layouts_key(workspace_id: WorkspaceId) -> String {
    i64::from(workspace_id).to_string()
}

fn saved_paths(paths: &PathList) -> Vec<String> {
    paths
        .paths()
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// Reads the layouts saved for a workspace. `paths` are the folders of the
/// workspace: the database gives the id of a deleted workspace to a new one,
/// which must not inherit layouts.
pub(crate) fn read_saved_layouts(
    workspace_id: WorkspaceId,
    paths: &PathList,
    cx: &App,
) -> Option<SavedLayouts> {
    let json = db::kvp::KeyValueStore::global(cx)
        .scoped(LAYOUTS_NAMESPACE)
        .read(&layouts_key(workspace_id))
        .log_err()
        .flatten()?;
    let saved = serde_json::from_str::<SavedLayouts>(&json).log_err()?;
    let is_valid = saved.paths == saved_paths(paths)
        && saved.layouts.iter().any(|layout| layout.id == saved.active);
    is_valid.then_some(saved)
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SavedLayouts {
    paths: Vec<String>,
    active: LayoutId,
    next_id: u64,
    layouts: Vec<SavedLayout>,
}

impl SavedLayouts {
    /// The tree of the layout the window showed.
    pub(crate) fn active_center(&self) -> Option<SerializedPaneGroup> {
        let layout = self
            .layouts
            .iter()
            .find(|layout| layout.id == self.active)?;
        Some(layout.center.clone().into_pane_group())
    }
}

#[derive(Serialize, Deserialize)]
struct SavedLayout {
    id: LayoutId,
    name: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    scope: Option<LayoutScope>,
    center: SavedMember,
}

#[derive(Clone, Serialize, Deserialize)]
enum SavedMember {
    Group {
        horizontal: bool,
        flexes: Vec<f32>,
        children: Vec<SavedMember>,
    },
    Pane {
        #[serde(default)]
        active: bool,
        pinned_count: usize,
        items: Vec<SavedItem>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedItem {
    kind: String,
    item_id: u64,
    active: bool,
    preview: bool,
}

impl SavedMember {
    fn into_pane_group(self) -> SerializedPaneGroup {
        match self {
            SavedMember::Group {
                horizontal,
                flexes,
                children,
            } => SerializedPaneGroup::Group {
                axis: SerializedAxis(if horizontal {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                }),
                flexes: Some(flexes),
                children: children
                    .into_iter()
                    .map(SavedMember::into_pane_group)
                    .collect(),
            },
            SavedMember::Pane {
                active,
                pinned_count,
                items,
            } => SerializedPaneGroup::Pane(SerializedPane::new(
                items
                    .into_iter()
                    .map(|item| SerializedItem {
                        kind: Arc::from(item.kind),
                        item_id: item.item_id,
                        active: item.active,
                        preview: item.preview,
                    })
                    .collect(),
                active,
                pinned_count,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        SERIALIZATION_THROTTLE_TIME, SplitDirection, WorkspaceDb, item::test::TestItem,
        register_serializable_item, tests::init_test,
    };
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use util::path;

    async fn new_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        init_test(cx);
        cx.update(|cx| register_serializable_item::<TestItem>(cx));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx))
    }

    fn add_item(
        workspace: &Entity<Workspace>,
        label: &str,
        cx: &mut VisualTestContext,
    ) -> Entity<TestItem> {
        let item = cx.new(|cx| TestItem::new(cx).with_label(label));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
        });
        item
    }

    fn tab_counts(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Vec<usize>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .layouts()
                .iter()
                .map(|layout| {
                    workspace
                        .layout_panes(layout.id())
                        .iter()
                        .map(|pane| pane.read(cx).items_len())
                        .collect()
                })
                .collect()
        })
    }

    #[gpui::test]
    async fn test_a_hidden_layout_keeps_its_tabs(cx: &mut TestAppContext) {
        let (workspace, cx) = new_workspace(cx).await;
        let main = workspace.read_with(cx, |workspace, _| workspace.active_layout());
        let first = add_item(&workspace, "first", cx);

        let review = workspace.update_in(cx, |workspace, window, cx| {
            workspace.create_layout("Review", "Checkout", window, cx)
        });
        let second = add_item(&workspace, "second", cx);

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.active_layout(), review);
            assert_eq!(workspace.layouts().len(), 2);
            assert_eq!(
                workspace.active_item(cx).map(|item| item.item_id()),
                Some(second.entity_id())
            );
            assert_eq!(workspace.layout_for_item(first.entity_id(), cx), Some(main));
            assert_eq!(
                workspace.layout_for_item(second.entity_id(), cx),
                Some(review)
            );
            // A closing window asks every layout about unsaved files.
            assert_eq!(workspace.items(cx).count(), 2);
            assert_eq!(workspace.shown_panes(), workspace.layout_panes(review));
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_layout(main, window, cx);
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.active_layout(), main);
            assert_eq!(
                workspace.active_item(cx).map(|item| item.item_id()),
                Some(first.entity_id())
            );
            assert!(
                workspace
                    .layout_panes(main)
                    .contains(workspace.active_pane())
            );
        });
        assert_eq!(tab_counts(&workspace, cx), vec![vec![1], vec![1]]);
    }

    #[gpui::test]
    async fn test_asking_for_a_tab_shows_its_layout(cx: &mut TestAppContext) {
        let (workspace, cx) = new_workspace(cx).await;
        let main = workspace.read_with(cx, |workspace, _| workspace.active_layout());
        let first = add_item(&workspace, "first", cx);
        let other_pane = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(
                workspace.active_pane().clone(),
                SplitDirection::Right,
                window,
                cx,
            )
        });
        let beside = add_item(&workspace, "beside", cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.create_layout("Review", "", window, cx);
        });
        add_item(&workspace, "second", cx);

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(workspace.activate_item(&first, true, true, window, cx));
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.active_layout(), main);
            assert_eq!(
                workspace.active_item(cx).map(|item| item.item_id()),
                Some(first.entity_id())
            );
            assert_ne!(workspace.active_pane(), &other_pane);
        });
        assert!(beside.read_with(cx, |_, _| true));
    }

    #[gpui::test]
    async fn test_closing_a_layout_closes_its_tabs(cx: &mut TestAppContext) {
        let (workspace, cx) = new_workspace(cx).await;
        let main = workspace.read_with(cx, |workspace, _| workspace.active_layout());
        add_item(&workspace, "first", cx);
        let review = workspace.update_in(cx, |workspace, window, cx| {
            workspace.create_layout("Review", "", window, cx)
        });
        add_item(&workspace, "second", cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_layout(main, window, cx);
        });

        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.remove_layout(review, window, cx)
            })
            .await
            .expect("the layout closes");
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.layouts().len(), 1);
            assert_eq!(workspace.active_layout(), main);
            assert_eq!(workspace.panes().len(), 1);
            assert_eq!(workspace.items(cx).count(), 1);
        });

        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.remove_layout(main, window, cx)
            })
            .await
            .expect("the last layout stays");
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.layouts().len(), 1);
            assert_eq!(workspace.items(cx).count(), 1);
        });
    }

    #[gpui::test]
    async fn test_an_emptied_pane_leaves_its_hidden_layout(cx: &mut TestAppContext) {
        let (workspace, cx) = new_workspace(cx).await;
        let main = workspace.read_with(cx, |workspace, _| workspace.active_layout());
        add_item(&workspace, "first", cx);
        let other_pane = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(
                workspace.active_pane().clone(),
                SplitDirection::Right,
                window,
                cx,
            )
        });
        let beside = add_item(&workspace, "beside", cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.create_layout("Review", "", window, cx);
        });
        assert_eq!(tab_counts(&workspace, cx), vec![vec![1, 1], vec![0]]);

        other_pane.update_in(cx, |pane, window, cx| {
            pane.remove_item(beside.entity_id(), false, true, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(tab_counts(&workspace, cx), vec![vec![1], vec![0]]);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.panes().len(), 2);
            assert_ne!(workspace.active_layout(), main);
        });
    }

    #[gpui::test]
    async fn test_layouts_come_back_with_their_splits_and_tabs(cx: &mut TestAppContext) {
        let (workspace, cx) = new_workspace(cx).await;
        let database = cx.update(|_, cx| WorkspaceDb::global(cx));
        let workspace_id = database.next_id().await.expect("the database gives an id");
        workspace.update(cx, |workspace, _| workspace.set_database_id(workspace_id));

        let first = add_item(&workspace, "first", cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(
                workspace.active_pane().clone(),
                SplitDirection::Right,
                window,
                cx,
            );
        });
        add_item(&workspace, "beside", cx);
        let review = workspace.update_in(cx, |workspace, window, cx| {
            workspace.create_layout("Review", "Checkout", window, cx)
        });
        add_item(&workspace, "second", cx);
        add_item(&workspace, "third", cx);

        cx.executor().advance_clock(SERIALIZATION_THROTTLE_TIME);
        cx.run_until_parked();
        let serialized_workspace = database
            .workspace_for_id(workspace_id)
            .expect("the workspace was saved");

        workspace
            .update_in(cx, |_, window, cx| {
                Workspace::load_workspace(serialized_workspace, Vec::new(), window, cx)
            })
            .await
            .expect("the workspace loads");
        cx.run_until_parked();

        assert_eq!(tab_counts(&workspace, cx), vec![vec![1, 1], vec![2]]);
        workspace.read_with(cx, |workspace, cx| {
            let layouts = workspace.layouts();
            assert_eq!(
                layouts
                    .iter()
                    .map(|layout| (layout.name().to_string(), layout.group().to_string()))
                    .collect::<Vec<_>>(),
                vec![
                    ("Main".to_string(), String::new()),
                    ("Review".to_string(), "Checkout".to_string()),
                ]
            );
            assert_eq!(workspace.active_layout(), review);
            assert!(workspace.layout_panes(review).contains(workspace.active_pane()));
            assert_eq!(workspace.panes().len(), 3);
            assert_eq!(workspace.items(cx).count(), 4);
            assert!(
                workspace.layout_for_item(first.entity_id(), cx).is_none(),
                "a restored tab is a new view"
            );
            assert!(matches!(
                workspace.layout_outline(layouts[0].id()),
                Some(LayoutOutline::Split { axis: Axis::Horizontal, ref children }) if children.len() == 2
            ));
        });

        // A layout made after a restart must not reuse an id.
        let third = workspace.update_in(cx, |workspace, window, cx| {
            workspace.create_layout("Third", "", window, cx)
        });
        assert_ne!(third, review);
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.layouts().len(), 3));
    }

    #[test]
    fn a_saved_layout_keeps_its_splits_and_items() {
        let saved = SavedMember::Group {
            horizontal: true,
            flexes: vec![2., 1.],
            children: vec![
                SavedMember::Pane {
                    active: true,
                    pinned_count: 1,
                    items: vec![SavedItem {
                        kind: "Editor".to_string(),
                        item_id: 7,
                        active: true,
                        preview: false,
                    }],
                },
                SavedMember::Pane {
                    active: false,
                    pinned_count: 0,
                    items: vec![SavedItem {
                        kind: "Terminal".to_string(),
                        item_id: 9,
                        active: true,
                        preview: false,
                    }],
                },
            ],
        };
        let json = serde_json::to_string(&saved).expect("a layout serializes");
        let restored = serde_json::from_str::<SavedMember>(&json)
            .expect("a layout deserializes")
            .into_pane_group();
        let SerializedPaneGroup::Group {
            axis,
            flexes,
            children,
        } = restored
        else {
            panic!("the split is lost");
        };
        assert_eq!(axis.0, Axis::Horizontal);
        assert_eq!(flexes, Some(vec![2., 1.]));
        assert_eq!(children.len(), 2);
        let SerializedPaneGroup::Pane(pane) = &children[0] else {
            panic!("the pane is lost");
        };
        assert!(pane.active);
        assert_eq!(pane.pinned_count, 1);
        assert_eq!(
            pane.children,
            vec![SerializedItem::new("Editor", 7, true, false)]
        );
    }
}
