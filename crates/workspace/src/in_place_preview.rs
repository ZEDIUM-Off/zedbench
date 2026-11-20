use crate::{Workspace, item::ItemHandle};
use gpui::{App, Context, EntityId, Global, Window};

/// Builds the preview of `item`, or returns `None` when `item` has none.
pub type InPlacePreviewProvider = fn(
    &mut Workspace,
    &dyn ItemHandle,
    &mut Window,
    &mut Context<Workspace>,
) -> Option<Box<dyn ItemHandle>>;

#[derive(Default)]
struct InPlacePreviews {
    providers: Vec<InPlacePreviewProvider>,
    pairs: Vec<InPlacePair>,
}

impl Global for InPlacePreviews {}

/// A file that one tab shows either as its source or as its preview. The pair
/// holds the one that is out of the pane, so switching back keeps its state:
/// the editor's selections and scroll, or the preview's own state.
struct InPlacePair {
    source: Box<dyn ItemHandle>,
    preview: Box<dyn ItemHandle>,
}

impl InPlacePair {
    fn contains(&self, item_id: EntityId) -> bool {
        self.source.item_id() == item_id || self.preview.item_id() == item_id
    }
}

pub fn register_in_place_preview(provider: InPlacePreviewProvider, cx: &mut App) {
    cx.default_global::<InPlacePreviews>()
        .providers
        .push(provider);
}

fn pair_of(item_id: EntityId, cx: &App) -> Option<&InPlacePair> {
    cx.try_global::<InPlacePreviews>()?
        .pairs
        .iter()
        .find(|pair| pair.contains(item_id))
}

/// Whether `item_id` is the source or the preview of a tab that switches
/// between them.
pub fn has_in_place_preview(item_id: EntityId, cx: &App) -> bool {
    pair_of(item_id, cx).is_some()
}

/// Whether `item_id` is the preview side of such a tab.
pub fn is_in_place_preview(item_id: EntityId, cx: &App) -> bool {
    pair_of(item_id, cx).is_some_and(|pair| pair.preview.item_id() == item_id)
}

/// The source that the preview `item_id` stands for in its tab.
pub(crate) fn in_place_source(item_id: EntityId, cx: &App) -> Option<Box<dyn ItemHandle>> {
    pair_of(item_id, cx)
        .filter(|pair| pair.preview.item_id() == item_id)
        .map(|pair| pair.source.boxed_clone())
}

impl Workspace {
    /// Switches the tab of `item` between the file's source and its preview.
    /// Returns `false` when `item` has no preview.
    pub fn toggle_preview_in_place(
        &mut self,
        item: &dyn ItemHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pane) = self.pane_for_item_id(item.item_id()) else {
            return false;
        };
        let counterpart = match pair_of(item.item_id(), cx) {
            Some(pair) if pair.source.item_id() == item.item_id() => pair.preview.boxed_clone(),
            Some(pair) => pair.source.boxed_clone(),
            None => {
                let providers = cx
                    .try_global::<InPlacePreviews>()
                    .map(|previews| previews.providers.clone())
                    .unwrap_or_default();
                let Some(preview) = providers
                    .into_iter()
                    .find_map(|provider| provider(self, item, window, cx))
                else {
                    return false;
                };
                cx.default_global::<InPlacePreviews>()
                    .pairs
                    .push(InPlacePair {
                        source: item.boxed_clone(),
                        preview: preview.boxed_clone(),
                    });
                preview
            }
        };
        pane.update(cx, |pane, cx| {
            let Some(index) = pane.index_for_item(item) else {
                return;
            };
            let focus = pane.has_focus(window, cx);
            let pinned = pane.is_tab_pinned(index);
            let temporary = pane.preview_item_id() == Some(item.item_id());
            let counterpart_id = counterpart.item_id();
            // The source goes out of the pane before its preview comes in:
            // `add_item` would otherwise treat an item of the same file as a
            // duplicate.
            pane.remove_item(item.item_id(), false, false, window, cx);
            pane.add_item(counterpart, focus, focus, Some(index), window, cx);
            if pinned {
                pane.pin_tab_at(index, window, cx);
            } else if temporary {
                pane.set_preview_item_id(Some(counterpart_id), cx);
            }
        });
        true
    }

    /// Drops the pairs whose tab was closed, which also releases the hidden
    /// side.
    pub(crate) fn forget_closed_in_place_previews(&self, removed: EntityId, cx: &mut App) {
        let Some(previews) = cx.try_global::<InPlacePreviews>() else {
            return;
        };
        let in_a_pane = |item_id: EntityId| {
            self.panes
                .iter()
                .any(|pane| pane.read(cx).index_for_item_id(item_id).is_some())
        };
        let closed = previews
            .pairs
            .iter()
            .map(|pair| {
                pair.contains(removed)
                    && !in_a_pane(pair.source.item_id())
                    && !in_a_pane(pair.preview.item_id())
            })
            .collect::<Vec<_>>();
        if !closed.contains(&true) {
            return;
        }
        let mut closed = closed.into_iter();
        cx.global_mut::<InPlacePreviews>()
            .pairs
            .retain(|_| !closed.next().unwrap_or(false));
    }
}
