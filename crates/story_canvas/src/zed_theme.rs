use gpui::{App, Global, Hsla};
use gpui_kit::component::{Theme as KitTheme, ThemeConfig};
use serde_json::{Map, Value, json};
use settings::SettingsStore;
use std::rc::Rc;
use theme::{ActiveTheme as _, GlobalTheme};

/// Keeps the GPUI Kit theme, which gpui-ai also reads, in step with Zed's
/// active theme and fonts.
pub fn init(cx: &mut App) {
    apply(cx);
    cx.observe_global::<GlobalTheme>(apply).detach();
    // Fonts live in the settings, and a font change does not touch the theme.
    cx.observe_global::<SettingsStore>(apply).detach();
}

/// The last configuration applied, so that settings changes which leave the
/// theme and fonts alone do not refresh every window.
struct AppliedConfig(Value);

impl Global for AppliedConfig {}

fn apply(cx: &mut App) {
    let value = zed_theme_config(cx);
    if cx
        .try_global::<AppliedConfig>()
        .is_some_and(|applied| applied.0 == value)
    {
        return;
    }
    let config = match serde_json::from_value::<ThemeConfig>(value.clone()) {
        Ok(config) => Rc::new(config),
        Err(error) => {
            log::error!("cannot convert the Zed theme for GPUI Kit: {error}");
            return;
        }
    };
    KitTheme::update(cx, |kit_theme| kit_theme.apply_config(&config));
    cx.set_global(AppliedConfig(value));
}

fn zed_theme_config(cx: &App) -> Value {
    let theme = cx.theme().clone();
    let zed_colors = theme.colors();
    let status = theme.status();
    let local = theme.players().local();
    let settings = theme::theme_settings(cx);
    let ui_font = settings.ui_font(cx).family.clone();
    let buffer_font = settings.buffer_font(cx).family.clone();
    let ui_font_size = f32::from(settings.ui_font_size(cx));
    let buffer_font_size = f32::from(settings.buffer_font_size(cx));
    let mode = if theme.appearance().is_light() {
        "light"
    } else {
        "dark"
    };

    let syntax = [
        "attribute",
        "boolean",
        "comment",
        "comment.doc",
        "constant",
        "constructor",
        "embedded",
        "emphasis",
        "emphasis.strong",
        "enum",
        "function",
        "hint",
        "keyword",
        "label",
        "link_text",
        "link_uri",
        "number",
        "operator",
        "predictive",
        "preproc",
        "primary",
        "property",
        "punctuation",
        "punctuation.bracket",
        "punctuation.delimiter",
        "punctuation.list_marker",
        "punctuation.special",
        "string",
        "string.escape",
        "string.regex",
        "string.special",
        "string.special.symbol",
        "tag",
        "tag.doctype",
        "text.literal",
        "title",
        "type",
        "variable",
        "variable.special",
        "variant",
    ]
    .into_iter()
    .filter_map(|name| {
        let color = theme.syntax().style_for_name(name)?.color?;
        // Kit names this capture with an underscore, Zed with a dot.
        let key = if name == "comment.doc" {
            "comment_doc"
        } else {
            name
        };
        Some((key.to_string(), json!({ "color": color })))
    })
    .collect::<Map<_, _>>();

    let colors = [
        ("background", zed_colors.editor_background),
        ("foreground", zed_colors.text),
        ("border", zed_colors.border),
        ("ring", zed_colors.border_focused),
        ("caret", local.cursor),
        ("selection.background", local.selection),
        ("link", zed_colors.text_accent),
        ("link.hover", zed_colors.link_text_hover),
        ("link.active", zed_colors.text_accent),
        ("muted.background", zed_colors.element_background),
        ("muted.foreground", zed_colors.text_muted),
        ("accent.background", zed_colors.element_hover),
        ("accent.foreground", zed_colors.text),
        ("primary.background", local.cursor),
        ("primary.hover.background", local.cursor.opacity(0.9)),
        ("primary.active.background", local.cursor.opacity(0.8)),
        ("primary.foreground", zed_colors.editor_background),
        ("secondary.background", zed_colors.element_background),
        ("secondary.hover.background", zed_colors.element_hover),
        ("secondary.active.background", zed_colors.element_active),
        ("secondary.foreground", zed_colors.text),
        ("popover.background", zed_colors.elevated_surface_background),
        ("popover.foreground", zed_colors.text),
        ("input.border", zed_colors.border_variant),
        ("drag.border", zed_colors.border_selected),
        ("drop_target.background", zed_colors.drop_target_background),
        ("list.background", zed_colors.editor_background),
        ("list.hover.background", zed_colors.ghost_element_hover),
        ("list.active.background", zed_colors.ghost_element_selected),
        ("list.active.border", zed_colors.border_selected),
        ("list.even.background", zed_colors.editor_background),
        ("list.head.background", zed_colors.surface_background),
        ("table.background", zed_colors.editor_background),
        ("table.hover.background", zed_colors.ghost_element_hover),
        ("table.active.background", zed_colors.ghost_element_selected),
        ("table.active.border", zed_colors.border_selected),
        ("table.even.background", zed_colors.editor_background),
        ("table.head.background", zed_colors.surface_background),
        ("table.head.foreground", zed_colors.text_muted),
        ("table.row.border", zed_colors.border_variant),
        ("tab_bar.background", zed_colors.tab_bar_background),
        ("tab.background", zed_colors.tab_inactive_background),
        ("tab.foreground", zed_colors.text_muted),
        ("tab.active.background", zed_colors.tab_active_background),
        ("tab.active.foreground", zed_colors.text),
        ("title_bar.background", zed_colors.title_bar_background),
        ("title_bar.border", zed_colors.border),
        ("status_bar.background", zed_colors.status_bar_background),
        ("status_bar.border", zed_colors.border),
        ("sidebar.background", zed_colors.panel_background),
        ("sidebar.foreground", zed_colors.text),
        ("sidebar.border", zed_colors.border),
        ("sidebar.accent.background", zed_colors.ghost_element_hover),
        ("sidebar.accent.foreground", zed_colors.text),
        ("sidebar.primary.background", local.cursor),
        ("sidebar.primary.foreground", zed_colors.editor_background),
        (
            "scrollbar.background",
            zed_colors.scrollbar_track_background,
        ),
        (
            "scrollbar.thumb.background",
            zed_colors.scrollbar_thumb_background,
        ),
        (
            "scrollbar.thumb.hover.background",
            zed_colors.scrollbar_thumb_hover_background,
        ),
        ("danger.background", status.error),
        ("danger.foreground", zed_colors.editor_background),
        ("success.background", status.success),
        ("success.foreground", zed_colors.editor_background),
        ("warning.background", status.warning),
        ("warning.foreground", zed_colors.editor_background),
        ("info.background", status.info),
        ("info.foreground", zed_colors.editor_background),
        ("chart.1", zed_colors.terminal_ansi_blue),
        ("chart.2", zed_colors.terminal_ansi_green),
        ("chart.3", zed_colors.terminal_ansi_yellow),
        ("chart.4", zed_colors.terminal_ansi_magenta),
        ("chart.5", zed_colors.terminal_ansi_cyan),
        ("chart.bullish", status.success),
        ("chart.bearish", status.error),
        ("chart.grid", zed_colors.border_variant),
        ("base.red", zed_colors.terminal_ansi_red),
        ("base.red.light", zed_colors.terminal_ansi_bright_red),
        ("base.green", zed_colors.terminal_ansi_green),
        ("base.green.light", zed_colors.terminal_ansi_bright_green),
        ("base.blue", zed_colors.terminal_ansi_blue),
        ("base.blue.light", zed_colors.terminal_ansi_bright_blue),
        ("base.yellow", zed_colors.terminal_ansi_yellow),
        ("base.yellow.light", zed_colors.terminal_ansi_bright_yellow),
        ("base.magenta", zed_colors.terminal_ansi_magenta),
        (
            "base.magenta.light",
            zed_colors.terminal_ansi_bright_magenta,
        ),
        ("base.cyan", zed_colors.terminal_ansi_cyan),
        ("base.cyan.light", zed_colors.terminal_ansi_bright_cyan),
    ];
    let mut highlight = color_map([
        ("editor.background", zed_colors.editor_background),
        ("editor.foreground", zed_colors.editor_foreground),
        (
            "editor.active_line.background",
            zed_colors.editor_active_line_background,
        ),
        ("editor.line_number", zed_colors.editor_line_number),
        (
            "editor.active_line_number",
            zed_colors.editor_active_line_number,
        ),
        ("editor.invisible", zed_colors.editor_invisible),
        (
            "editor.gutter.background",
            zed_colors.editor_gutter_background,
        ),
        ("error", status.error),
        ("error.background", status.error_background),
        ("error.border", status.error_border),
        ("warning", status.warning),
        ("warning.background", status.warning_background),
        ("warning.border", status.warning_border),
        ("info", status.info),
        ("info.background", status.info_background),
        ("info.border", status.info_border),
        ("success", status.success),
        ("success.background", status.success_background),
        ("success.border", status.success_border),
        ("hint", status.hint),
        ("hint.background", status.hint_background),
        ("hint.border", status.hint_border),
    ]);
    highlight.insert("syntax".into(), Value::Object(syntax));

    json!({
        "name": format!("Zedbench {}", theme.name),
        "mode": mode,
        "font.family": ui_font,
        "font.size": ui_font_size,
        "mono_font.family": buffer_font,
        "mono_font.size": buffer_font_size,
        "colors": color_map(colors),
        "highlight": highlight,
    })
}

fn color_map<const N: usize>(pairs: [(&str, Hsla); N]) -> Map<String, Value> {
    pairs
        .into_iter()
        .map(|(key, color)| (key.to_string(), json!(color)))
        .collect()
}
