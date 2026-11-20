use std::collections::HashMap;
use std::str::FromStr as _;
use std::time::Duration;

use gpui::{
    AnyElement, AnyView, ClickEvent, Context, Entity, Hsla, IntoElement, Render, SharedString,
    Subscription, Task, Window, anchored, deferred, div, prelude::*, px, svg,
};
use gpui_kit::component::{
    Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
    progress::Progress,
    switch::Switch,
    tab::{Tab, TabBar as KitTabBar},
    tag::Tag,
};
use icons::IconName;
use openui::{ActionPlan, ActionStep, Document, Element, Inputs, Library, Severity, Value};
use theme::ActiveTheme as _;
use util::ResultExt as _;
use workspace::ui::{Tab as ZedTab, TabBar, TabPosition, Toggleable as _};

/// How long the pointer rests on a trigger before its hover card shows, so
/// crossing a list of triggers does not flash a card for each.
const HOVER_CARD_DELAY: Duration = Duration::from_millis(250);

/// A story written in OpenUI Lang, interpreted on every render, so editing
/// the file updates the preview without compiling anything.
pub(crate) struct OpenUiStory {
    library: Library,
    source: String,
    document: Document,
    /// One document per slot mounted in the window: the story whose root is
    /// that slot. Every slot reads and writes the same state.
    slots: HashMap<String, Document>,
    /// The slot being rendered, so its actions resolve in its own document.
    scope: Option<String>,
    /// State the story changed through actions and bindings. Variables it
    /// never touched read their declared defaults.
    state: HashMap<String, Value>,
    inputs: HashMap<SharedString, (Entity<InputState>, Subscription)>,
    embedded: HashMap<String, Result<AnyView, SharedString>>,
    /// The hover card whose trigger is under the pointer.
    hovered: Option<SharedString>,
    _show_hover_card: Option<Task<()>>,
    notice: Option<SharedString>,
}

impl OpenUiStory {
    pub(crate) fn new(library: &Library, source: &str) -> Self {
        Self {
            library: library.clone(),
            source: source.to_owned(),
            document: openui::parse(library, source),
            slots: HashMap::default(),
            scope: None,
            state: HashMap::default(),
            inputs: HashMap::default(),
            embedded: HashMap::default(),
            hovered: None,
            _show_hover_card: None,
            notice: None,
        }
    }

    /// Reparses the story and keeps its state, so an edit does not reset
    /// what the user clicked through.
    pub(crate) fn set_source(&mut self, library: &Library, source: &str, cx: &mut Context<Self>) {
        self.library = library.clone();
        self.source = source.to_owned();
        self.document = openui::parse(library, source);
        for (slot, document) in &mut self.slots {
            *document = openui::parse(library, &slot_source(source, slot));
        }
        self.notice = None;
        cx.notify();
    }

    /// Whether the story defines `name` at its top level.
    pub(crate) fn defines(&self, name: &str) -> bool {
        self.document
            .statements()
            .iter()
            .any(|statement| statement.name() == name)
    }

    /// Renders one slot of the story, for a part of the window it is
    /// mounted in.
    pub(crate) fn render_slot(
        &mut self,
        slot: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let document = self
            .slots
            .entry(slot.to_owned())
            .or_insert_with(|| openui::parse(&self.library, &slot_source(&self.source, slot)))
            .clone();
        let evaluation = document.evaluate(&self.inputs());
        let errors = evaluation
            .diagnostics()
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Error)
            .map(|diagnostic| diagnostic.to_string())
            .collect::<Vec<_>>();
        self.scope = Some(slot.to_owned());
        let body = match evaluation.root() {
            Some(root) => self.render_element(root, slot, window, cx),
            None => error_line(format!("The story defines no {slot}").into(), cx),
        };
        self.scope = None;
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_color(cx.theme().colors().text)
            .children(errors.into_iter().map(|error| error_line(error.into(), cx)))
            .child(body)
            .into_any_element()
    }

    /// Evaluates a slot and reads the element it renders, if any.
    fn read_slot<R>(&mut self, slot: &str, read: impl FnOnce(Option<&Element>) -> R) -> R {
        let document = self
            .slots
            .entry(slot.to_owned())
            .or_insert_with(|| openui::parse(&self.library, &slot_source(&self.source, slot)))
            .clone();
        let evaluation = document.evaluate(&self.inputs());
        let children = evaluation
            .root()
            .and_then(|root| root.prop("children"))
            .and_then(Value::as_array);
        let content: Option<&Element> = match children.and_then(|children| children.first()) {
            Some(Value::Element(content)) => Some(content),
            _ => None,
        };
        read(content)
    }

    /// The width a slot asks for in its own style, for the parts of the
    /// window that have no size of their own.
    pub(crate) fn slot_width(&mut self, slot: &str) -> Option<f32> {
        self.read_slot(slot, |content| match content?.prop("style") {
            Some(Value::Object(style)) => style
                .get("width")
                .and_then(Value::as_f64)
                .map(|width| width as f32),
            _ => None,
        })
    }

    /// Whether a slot renders nothing: no element, or a container without
    /// children. A story closes its modal this way.
    pub(crate) fn slot_is_empty(&mut self, slot: &str) -> bool {
        self.read_slot(slot, |content| match content {
            Some(content) => {
                matches!(content.component(), "Column" | "Row")
                    && content
                        .prop("children")
                        .and_then(Value::as_array)
                        .is_none_or(|children| children.is_empty())
            }
            None => true,
        })
    }

    fn inputs(&self) -> Inputs {
        let mut inputs = Inputs::new();
        for (name, value) in &self.state {
            inputs.set_state(name.clone(), value.clone());
        }
        inputs
    }

    fn run(&mut self, scope: Option<&str>, plan: &ActionPlan, cx: &mut Context<Self>) {
        let document = scope
            .and_then(|slot| self.slots.get(slot))
            .unwrap_or(&self.document)
            .clone();
        for step in plan.steps() {
            match step {
                ActionStep::Set { target, value } => {
                    let value = document.resolve(value, &self.inputs());
                    self.state.insert(target.clone(), value);
                }
                ActionStep::Reset { targets } => {
                    for target in targets {
                        self.state.remove(target);
                    }
                }
                ActionStep::ToAssistant { message, .. } => {
                    self.notice = Some(format!("Would send to the assistant: {message}").into());
                }
                ActionStep::OpenUrl { url } => {
                    self.notice = Some(format!("Would open {url}").into());
                }
                other => {
                    self.notice = Some(format!("Stories do not run {other:?}").into());
                }
            }
        }
        cx.notify();
    }

    fn set_state(&mut self, name: &str, value: Value, cx: &mut Context<Self>) {
        if self.state.get(name) != Some(&value) {
            self.state.insert(name.to_owned(), value);
            cx.notify();
        }
    }

    fn render_value(
        &mut self,
        value: &Value,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        match value {
            Value::Element(element) => vec![self.render_element(element, path, window, cx)],
            Value::Array(items) => items
                .iter()
                .enumerate()
                .flat_map(|(index, item)| {
                    self.render_value(item, &format!("{path}.{index}"), window, cx)
                })
                .collect(),
            Value::Null => Vec::new(),
            other => vec![div().child(other.to_js_string()).into_any_element()],
        }
    }

    fn render_element(
        &mut self,
        element: &Element,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(path.to_owned());
        let text = |name: &str| prop_str(element, name).unwrap_or_default().to_owned();
        let colors = cx.theme().colors().clone();
        match element.component() {
            "Column" | "Row" => {
                let mut container = div().id(id).flex();
                container = if element.component() == "Row" {
                    container.flex_row().items_center()
                } else {
                    container.flex_col()
                };
                if let Some(Value::Object(style)) = element.prop("style") {
                    let number = |key: &str| style.get(key).and_then(Value::as_f64);
                    let flag = |key: &str| style.get(key).and_then(Value::as_bool) == Some(true);
                    if let Some(gap) = number("gap") {
                        container = container.gap(px(gap as f32));
                    }
                    if let Some(padding) = number("padding") {
                        container = container.p(px(padding as f32));
                    }
                    if let Some(width) = number("width") {
                        container = container.w(px(width as f32)).flex_none();
                    }
                    if let Some(height) = number("height") {
                        container = container.h(px(height as f32)).flex_none();
                    }
                    if flag("grow") {
                        container = container.flex_1().flex_basis(px(0.)).min_w_0().min_h_0();
                    }
                    if let Some(flex) = number("flex") {
                        container = container.flex_basis(px(0.)).min_w_0().min_h_0();
                        container.style().flex_grow = Some(flex.max(0.) as f32);
                    }
                    if flag("wrap") {
                        container = container.flex_wrap();
                    }
                    if flag("clip") {
                        container = container.overflow_hidden();
                    }
                    if flag("scroll") {
                        container = if element.component() == "Row" {
                            container.overflow_x_scroll()
                        } else {
                            container.overflow_y_scroll()
                        };
                    }
                    if flag("border") {
                        container = container.border_1().border_color(colors.border);
                    }
                    if flag("rounded") {
                        container = container.rounded_md();
                    }
                    match style.get("surface").and_then(Value::as_str) {
                        Some("panel") => container = container.bg(colors.panel_background),
                        Some("elevated") => {
                            container = container.bg(colors.elevated_surface_background)
                        }
                        Some("editor") => container = container.bg(colors.editor_background),
                        Some("selected") => container = container.bg(colors.element_selected),
                        Some("title_bar") => container = container.bg(colors.title_bar_background),
                        Some("status_bar") => {
                            container = container.bg(colors.status_bar_background)
                        }
                        Some("tab_bar") => container = container.bg(colors.tab_bar_background),
                        Some("terminal") => container = container.bg(colors.terminal_background),
                        Some("muted") => container = container.bg(colors.element_background),
                        Some("accent") => container = container.bg(colors.text_accent),
                        Some("success") => container = container.bg(cx.theme().status().success),
                        Some("warning") => container = container.bg(cx.theme().status().warning),
                        Some("error") => container = container.bg(cx.theme().status().error),
                        Some("info") => container = container.bg(cx.theme().status().info),
                        _ => {}
                    }
                    match style.get("align").and_then(Value::as_str) {
                        Some("start") => container = container.items_start(),
                        Some("center") => container = container.items_center(),
                        Some("end") => container = container.items_end(),
                        Some("stretch") => container = container.items_stretch(),
                        _ => {}
                    }
                    match style.get("justify").and_then(Value::as_str) {
                        Some("start") => container = container.justify_start(),
                        Some("center") => container = container.justify_center(),
                        Some("end") => container = container.justify_end(),
                        Some("between") => container = container.justify_between(),
                        _ => {}
                    }
                }
                if element.prop("selected").and_then(Value::as_bool) == Some(true) {
                    container = container.bg(colors.element_selected);
                }
                if let Some(plan) = element.prop("action").and_then(Value::as_action) {
                    let plan = plan.clone();
                    let scope = self.scope.clone();
                    container = container
                        .role(gpui::Role::Button)
                        .aria_label(shown_text(element))
                        .cursor_pointer()
                        .hover(|container| container.bg(colors.ghost_element_hover))
                        .on_click(cx.listener(move |story, _: &ClickEvent, _, cx| {
                            story.run(scope.as_deref(), &plan, cx)
                        }));
                }
                let children = element
                    .prop("children")
                    .map(|children| self.render_value(children, path, window, cx))
                    .unwrap_or_default();
                container.children(children).into_any_element()
            }
            "HoverCard" => {
                let trigger = element
                    .prop("trigger")
                    .map(|trigger| {
                        self.render_value(trigger, &format!("{path}.trigger"), window, cx)
                    })
                    .unwrap_or_default();
                let card = (self.hovered.as_ref() == Some(&id))
                    .then(|| element.prop("content"))
                    .flatten()
                    .map(|content| self.render_value(content, &format!("{path}.card"), window, cx));
                let below = prop_str(element, "side") == Some("bottom");
                div()
                    .id(id.clone())
                    .relative()
                    .on_hover(cx.listener(move |story, hovered: &bool, _, cx| {
                        if *hovered {
                            let id = id.clone();
                            story._show_hover_card = Some(cx.spawn(async move |story, cx| {
                                cx.background_executor().timer(HOVER_CARD_DELAY).await;
                                story
                                    .update(cx, |story, cx| {
                                        story.hovered = Some(id);
                                        cx.notify();
                                    })
                                    .log_err();
                            }));
                        } else {
                            story._show_hover_card = None;
                            if story.hovered.as_ref() == Some(&id) {
                                story.hovered = None;
                                cx.notify();
                            }
                        }
                    }))
                    .children(trigger)
                    .when_some(card, |trigger, card| {
                        let card = div()
                            .p_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.elevated_surface_background)
                            .shadow_lg()
                            .children(card);
                        // The card is drawn after the window, anchored to the
                        // trigger's edge, so a dock never clips it.
                        trigger.child(
                            div()
                                .absolute()
                                .map(|anchor| {
                                    if below {
                                        anchor.left_0().top_full().pt_1()
                                    } else {
                                        anchor.top_0().left_full().pl_1()
                                    }
                                })
                                .child(
                                    deferred(
                                        anchored().snap_to_window_with_margin(px(8.)).child(card),
                                    )
                                    .with_priority(1),
                                ),
                        )
                    })
                    .into_any_element()
            }
            "Text" => {
                let variant = prop_str(element, "variant").unwrap_or("body");
                let mut label = div().child(text("text"));
                label = match variant {
                    "heading" => label.text_lg().font_weight(gpui::FontWeight::SEMIBOLD),
                    "title" => label.text_xl().font_weight(gpui::FontWeight::BOLD),
                    "small" => label.text_xs().text_color(colors.text_muted),
                    "code" => label.font_family(buffer_font(cx)).text_sm(),
                    _ => label,
                };
                if let Some(color) = tone_color(variant, cx) {
                    label = label.text_color(color);
                }
                if element.prop("truncate").and_then(Value::as_bool) == Some(true) {
                    label = label.min_w_0().truncate();
                }
                label.into_any_element()
            }
            "Code" => div()
                .font_family(buffer_font(cx))
                .text_sm()
                // Terminal output keeps its lines; a narrow pane clips them
                // instead of breaking every word.
                .whitespace_nowrap()
                .overflow_hidden()
                .min_w_0()
                .p_2()
                .rounded_md()
                .bg(colors.editor_background)
                .children(
                    text("text")
                        .lines()
                        .map(|line| div().child(line.to_owned())),
                )
                .into_any_element(),
            "Icon" => {
                let color = prop_str(element, "variant")
                    .and_then(|variant| tone_color(variant, cx))
                    .unwrap_or(colors.text);
                icon(&text("name"), color)
            }
            "Button" => {
                let mut button = Button::new(id).label(text("label")).small();
                button = match prop_str(element, "variant") {
                    Some("primary") => button.primary(),
                    Some("ghost") => button.ghost(),
                    Some("danger") => button.danger(),
                    Some("outline") => button.outline(),
                    _ => button,
                };
                if element.prop("selected").and_then(Value::as_bool) == Some(true) {
                    button = button.selected(true);
                }
                if let Some(icon_name) = prop_str(element, "icon") {
                    button = button.child(icon(icon_name, colors.text_muted));
                }
                if let Some(plan) = element.prop("action").and_then(Value::as_action) {
                    let plan = plan.clone();
                    let scope = self.scope.clone();
                    button = button.on_click(cx.listener(move |story, _: &ClickEvent, _, cx| {
                        story.run(scope.as_deref(), &plan, cx)
                    }));
                }
                button.into_any_element()
            }
            "ListItem" => {
                let selected = element.prop("selected").and_then(Value::as_bool) == Some(true);
                let depth = element.prop("depth").and_then(Value::as_f64).unwrap_or(0.);
                let trailing = element
                    .prop("trailing")
                    .map(|trailing| self.render_value(trailing, path, window, cx))
                    .unwrap_or_default();
                let mut row = div()
                    .id(id)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .py_0p5()
                    .pr_2()
                    .pl(px(8. + 14. * depth.max(0.) as f32))
                    .rounded_sm()
                    .hover(|row| row.bg(colors.ghost_element_hover))
                    .when(selected, |row| row.bg(colors.element_selected));
                if let Some(icon_name) = prop_str(element, "icon") {
                    row = row.child(icon(icon_name, colors.text_muted));
                }
                let mut label = div().min_w_0().truncate().child(text("label"));
                label.style().flex_grow = Some(1.);
                let detail = prop_str(element, "detail").map(|detail| {
                    let mut detail = div()
                        .min_w_0()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .truncate()
                        .child(detail.to_owned());
                    // The label keeps its width: in a narrow row, the detail
                    // gives way first.
                    detail.style().flex_shrink = Some(1000.);
                    detail
                });
                row = row.child(label).children(detail).children(trailing);
                if let Some(plan) = element.prop("action").and_then(Value::as_action) {
                    let plan = plan.clone();
                    let scope = self.scope.clone();
                    row = row
                        .role(gpui::Role::Button)
                        .aria_label(text("label"))
                        .cursor_pointer()
                        .on_click(cx.listener(move |story, _: &ClickEvent, _, cx| {
                            story.run(scope.as_deref(), &plan, cx)
                        }));
                }
                row.into_any_element()
            }
            "Tag" => {
                let tag = match prop_str(element, "variant") {
                    Some("primary") => Tag::primary(),
                    Some("secondary") => Tag::secondary(),
                    Some("danger") => Tag::danger(),
                    Some("success") => Tag::success(),
                    Some("warning") => Tag::warning(),
                    Some("info") => Tag::info(),
                    _ => Tag::new(),
                };
                tag.small().child(text("text")).into_any_element()
            }
            "Input" => {
                let binding = binding(element, "value");
                let value = text("value");
                let placeholder = text("placeholder");
                let state = self.input_state(&id, binding, &placeholder, window, cx);
                if state.read(cx).value().as_ref() != value {
                    state.update(cx, |state, cx| state.set_value(value, window, cx));
                }
                Input::new(&state).small().into_any_element()
            }
            "Switch" | "Checkbox" => {
                let checked = element.prop("checked").and_then(Value::as_bool) == Some(true);
                let handler = binding(element, "checked").map(|binding| {
                    cx.listener(move |story, checked: &bool, _, cx| {
                        story.set_state(&binding, Value::from(*checked), cx)
                    })
                });
                if element.component() == "Switch" {
                    Switch::new(id)
                        .label(text("label"))
                        .checked(checked)
                        .when_some(handler, |switch, handler| switch.on_click(handler))
                        .into_any_element()
                } else {
                    Checkbox::new(id)
                        .label(text("label"))
                        .checked(checked)
                        .when_some(handler, |checkbox, handler| checkbox.on_click(handler))
                        .into_any_element()
                }
            }
            "Tabs" => {
                let labels = element
                    .prop("labels")
                    .and_then(Value::as_array)
                    .unwrap_or_default()
                    .iter()
                    .map(Value::to_js_string)
                    .collect::<Vec<_>>();
                let selected = element
                    .prop("selected")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.)
                    .max(0.) as usize;
                let mut tabs = KitTabBar::new(id)
                    .small()
                    .children(labels.into_iter().map(|label| Tab::new().label(label)))
                    .selected_index(selected);
                if let Some(binding) = binding(element, "selected") {
                    tabs = tabs.on_click(cx.listener(move |story, index: &usize, _, cx| {
                        story.set_state(&binding, Value::from(*index as f64), cx)
                    }));
                }
                tabs.into_any_element()
            }
            "PaneTabs" => {
                let selected = element
                    .prop("selected")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.)
                    .max(0.) as usize;
                let binding = binding(element, "selected");
                let tabs = element
                    .prop("tabs")
                    .and_then(Value::as_array)
                    .unwrap_or_default()
                    .to_vec();
                let count = tabs.len();
                let end = element
                    .prop("end")
                    .map(|end| self.render_value(end, path, window, cx))
                    .unwrap_or_default();
                let mut bar = TabBar::new(id);
                for (index, tab) in tabs.iter().enumerate() {
                    let field = |name: &str| {
                        tab.as_object()
                            .and_then(|tab| tab.get(name))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    };
                    let dirty = tab
                        .as_object()
                        .and_then(|tab| tab.get("dirty"))
                        .and_then(Value::as_bool)
                        == Some(true);
                    let position = if index == 0 {
                        TabPosition::First
                    } else if index + 1 == count {
                        TabPosition::Last
                    } else {
                        TabPosition::Middle(index.cmp(&selected))
                    };
                    let mut zed_tab =
                        ZedTab::new(SharedString::from(format!("{path}.tab.{index}")))
                            .position(position)
                            .toggle_state(index == selected)
                            .start_slot::<AnyElement>(
                                field("icon").map(|name| icon(&name, colors.text_muted)),
                            )
                            .end_slot::<AnyElement>(dirty.then(|| {
                                div()
                                    .size_1p5()
                                    .rounded_full()
                                    .bg(colors.text_accent)
                                    .into_any_element()
                            }))
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(field("label").unwrap_or_default())
                                    .when_some(field("detail"), |label, detail| {
                                        label.child(
                                            div().text_color(colors.text_muted).child(detail),
                                        )
                                    }),
                            );
                    if let Some(binding) = binding.clone() {
                        zed_tab =
                            zed_tab.on_click(cx.listener(move |story, _: &ClickEvent, _, cx| {
                                story.set_state(&binding, Value::from(index as f64), cx)
                            }));
                    }
                    bar = bar.child(zed_tab);
                }
                bar.end_children(end).into_any_element()
            }
            "Editor" => {
                let first_line = element
                    .prop("firstLine")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.)
                    .max(1.) as usize;
                let active_line = element.prop("activeLine").and_then(Value::as_f64);
                div()
                    .id(id)
                    .size_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .py_1()
                    .bg(colors.editor_background)
                    .font_family(buffer_font(cx))
                    .text_sm()
                    .children(text("text").lines().enumerate().map(|(index, line)| {
                        let number = first_line + index;
                        let active = active_line.map(|line| line as usize) == Some(number);
                        div()
                            .flex()
                            .flex_none()
                            .whitespace_nowrap()
                            .when(active, |row| row.bg(colors.editor_active_line_background))
                            .child(
                                div()
                                    .w(px(44.))
                                    .flex_none()
                                    .pr_3()
                                    .text_right()
                                    .text_color(if active {
                                        colors.editor_active_line_number
                                    } else {
                                        colors.editor_line_number
                                    })
                                    .child(number.to_string()),
                            )
                            .child(line.to_owned())
                    }))
                    .into_any_element()
            }
            "Terminal" => div()
                .id(id)
                .size_full()
                .overflow_hidden()
                .p_2()
                .bg(colors.terminal_background)
                .text_color(colors.terminal_foreground)
                .font_family(buffer_font(cx))
                .text_sm()
                .whitespace_nowrap()
                .children(
                    text("text")
                        .lines()
                        .map(|line| div().child(line.to_owned())),
                )
                .into_any_element(),
            "Progress" => Progress::new(id)
                .value(element.prop("value").and_then(Value::as_f64).unwrap_or(0.) as f32)
                .into_any_element(),
            "Divider" => div()
                .flex_none()
                .min_h(px(1.))
                .min_w(px(1.))
                .bg(colors.border_variant)
                .into_any_element(),
            "Spacer" => div().flex_1().into_any_element(),
            "KitStory" | "AiStory" => {
                let key = format!("{}:{}", element.component(), text("name"));
                let embedded = match self.embedded.get(&key) {
                    Some(embedded) => embedded.clone(),
                    None => {
                        let embedded =
                            embedded_story(element.component(), &text("name"), window, cx);
                        self.embedded.insert(key, embedded.clone());
                        embedded
                    }
                };
                match embedded {
                    Ok(view) => div().size_full().child(view).into_any_element(),
                    Err(error) => error_line(error, cx),
                }
            }
            other => error_line(format!("Unknown component {other}").into(), cx),
        }
    }

    fn input_state(
        &mut self,
        id: &SharedString,
        binding: Option<String>,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some((state, _)) = self.inputs.get(id) {
            return state.clone();
        }
        let placeholder = placeholder.to_owned();
        let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe(&state, move |story, state, event: &InputEvent, cx| {
            if let (InputEvent::Change, Some(binding)) = (event, &binding) {
                let value = state.read(cx).value().to_string();
                story.set_state(binding, Value::from(value), cx);
            }
        });
        self.inputs
            .insert(id.clone(), (state.clone(), subscription));
        state
    }
}

impl Render for OpenUiStory {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let evaluation = self.document.evaluate(&self.inputs());
        let errors = self
            .document
            .diagnostics()
            .iter()
            .chain(evaluation.diagnostics())
            .filter(|diagnostic| diagnostic.severity == Severity::Error)
            .map(|diagnostic| diagnostic.to_string())
            .collect::<Vec<_>>();
        let body = match evaluation.root() {
            Some(root) => self.render_element(root, "root", window, cx),
            None if errors.is_empty() => error_line("The story renders nothing".into(), cx),
            None => div().into_any_element(),
        };
        let colors = cx.theme().colors().clone();
        let status = cx.theme().status().clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_color(colors.text)
            .when(!errors.is_empty(), |story| {
                story.child(
                    div()
                        .flex_none()
                        .p_2()
                        .border_b_1()
                        .border_color(status.error_border)
                        .bg(status.error_background)
                        .text_sm()
                        .children(errors.into_iter().map(|error| div().child(error))),
                )
            })
            .when_some(self.notice.clone(), |story, notice| {
                story.child(
                    div()
                        .flex_none()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .bg(status.info_background)
                        .child(notice),
                )
            })
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
    }
}

/// The story with `slot` as its root, so it can render alone.
fn slot_source(source: &str, slot: &str) -> String {
    format!("{source}\nroot = Column([{slot}], {{grow: true}})\n")
}

/// The text a clickable container shows. It names the container for a tool
/// that reads the window instead of looking at it.
fn shown_text(element: &Element) -> String {
    fn collect(value: &Value, words: &mut Vec<String>) {
        match value {
            Value::Element(element) if matches!(element.component(), "Text" | "Tag") => {
                if let Some(text) = prop_str(element, "text").filter(|text| !text.is_empty()) {
                    words.push(text.to_owned());
                }
            }
            Value::Element(element) => {
                if let Some(children) = element.prop("children") {
                    collect(children, words);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect(item, words)),
            _ => {}
        }
    }
    let mut words = Vec::new();
    if let Some(children) = element.prop("children") {
        collect(children, &mut words);
    }
    words.join(" ")
}

fn prop_str<'a>(element: &'a Element, name: &str) -> Option<&'a str> {
    element.prop(name).and_then(Value::as_str)
}

fn binding(element: &Element, name: &str) -> Option<String> {
    element
        .props()
        .iter()
        .find(|prop| prop.name() == name)
        .and_then(|prop| prop.binding())
        .map(str::to_owned)
}

fn tone_color(variant: &str, cx: &Context<OpenUiStory>) -> Option<Hsla> {
    let colors = cx.theme().colors();
    let status = cx.theme().status();
    match variant {
        "muted" => Some(colors.text_muted),
        "accent" => Some(colors.text_accent),
        "error" => Some(status.error),
        "success" => Some(status.success),
        "warning" => Some(status.warning),
        _ => None,
    }
}

fn buffer_font(cx: &Context<OpenUiStory>) -> SharedString {
    theme::theme_settings(cx).buffer_font(cx).family.clone()
}

fn icon(name: &str, color: Hsla) -> AnyElement {
    match IconName::from_str(name) {
        Ok(icon) => svg()
            .path(icon.path())
            .size_4()
            .flex_none()
            .text_color(color)
            .into_any_element(),
        Err(_) => div().child(format!("[{name}]")).into_any_element(),
    }
}

fn error_line(error: SharedString, cx: &Context<OpenUiStory>) -> AnyElement {
    div()
        .p_2()
        .text_color(cx.theme().status().error)
        .child(error)
        .into_any_element()
}

fn embedded_story(
    component: &str,
    name: &str,
    window: &mut Window,
    cx: &mut Context<OpenUiStory>,
) -> Result<AnyView, SharedString> {
    if component == "KitStory" {
        return gpui_component_story::open_story(name, window, cx)
            .map(AnyView::from)
            .ok_or_else(|| {
                format!(
                    "Unknown GPUI Kit story {name:?}. Known stories: {}",
                    gpui_component_story::catalog()
                        .iter()
                        .map(|(title, _)| *title)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into()
            });
    }
    match name.trim().parse::<gpui_ai_gallery::StoryId>() {
        Ok(id) => Ok(gpui_ai_gallery::embedded_story(id, cx).into()),
        Err(error) => Err(format!(
            "{error}. Known gpui-ai stories: {}",
            gpui_ai_gallery::StoryId::ALL
                .iter()
                .map(|id| id.slug())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into()),
    }
}
