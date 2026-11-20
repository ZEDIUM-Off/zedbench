//! The components a `.gpui-story` file, or an agent, can use. It depends only on
//! `openui`, so tools can check stories without building GPUI.

use openui::{ComponentDef, Library, LibraryError, Param, Type};

const TEXT_VARIANTS: [&str; 10] = [
    "body", "heading", "title", "muted", "small", "code", "accent", "error", "success", "warning",
];
const TONES: [&str; 7] = [
    "default",
    "primary",
    "secondary",
    "danger",
    "success",
    "warning",
    "info",
];

/// The components a story, or an agent, can use. Their signatures double as
/// the prompt that tells a model what it may write.
pub fn library() -> Result<Library, LibraryError> {
    let style = Type::object([
        Param::optional("gap", Type::Number),
        Param::optional("padding", Type::Number),
        Param::optional("width", Type::Number),
        Param::optional("height", Type::Number),
        Param::optional("grow", Type::Boolean),
        Param::optional("scroll", Type::Boolean),
        Param::optional("border", Type::Boolean),
        Param::optional("rounded", Type::Boolean),
        Param::optional(
            "surface",
            Type::one_of(["none", "panel", "elevated", "editor", "selected"]),
        ),
        Param::optional("align", Type::one_of(["start", "center", "end", "stretch"])),
        Param::optional(
            "justify",
            Type::one_of(["start", "center", "end", "between"]),
        ),
    ]);
    let children = || Param::required("children", Type::list(Type::component()));
    Library::builder()
        .component(
            ComponentDef::new("Column", "Lays out children top to bottom")
                .param(children())
                .param(Param::optional("style", style.clone())),
        )
        .component(
            ComponentDef::new("Row", "Lays out children left to right")
                .param(children())
                .param(Param::optional("style", style)),
        )
        .component(
            ComponentDef::new("Text", "A run of text")
                .param(Param::required("text", Type::String))
                .param(Param::optional("variant", Type::one_of(TEXT_VARIANTS))),
        )
        .component(
            ComponentDef::new("Code", "Monospace text that keeps its line breaks")
                .param(Param::required("text", Type::String)),
        )
        .component(
            ComponentDef::new(
                "Icon",
                "A Zed icon, by snake_case name such as \"terminal\"",
            )
            .param(Param::required("name", Type::String))
            .param(Param::optional("variant", Type::one_of(TEXT_VARIANTS))),
        )
        .component(
            ComponentDef::new("Button", "A clickable button")
                .param(Param::required("label", Type::String))
                .param(Param::optional("action", Type::action()))
                .param(Param::optional(
                    "variant",
                    Type::one_of(["default", "primary", "ghost", "danger", "outline"]),
                ))
                .param(Param::optional("selected", Type::Boolean))
                .param(Param::optional("icon", Type::String)),
        )
        .component(
            ComponentDef::new(
                "ListItem",
                "A clickable row for trees and lists, indented by depth",
            )
            .param(Param::required("label", Type::String))
            .param(Param::optional("action", Type::action()))
            .param(Param::optional("selected", Type::Boolean))
            .param(Param::optional("icon", Type::String))
            .param(Param::optional("detail", Type::String))
            .param(Param::optional("depth", Type::Number))
            .param(Param::optional("trailing", Type::list(Type::component()))),
        )
        .component(
            ComponentDef::new("Tag", "A small label")
                .param(Param::required("text", Type::String))
                .param(Param::optional("variant", Type::one_of(TONES))),
        )
        .component(
            ComponentDef::new("Input", "A single-line text field")
                .param(Param::required("value", Type::String).bindable())
                .param(Param::optional("placeholder", Type::String)),
        )
        .component(
            ComponentDef::new("Switch", "An on/off switch")
                .param(Param::required("label", Type::String))
                .param(Param::required("checked", Type::Boolean).bindable()),
        )
        .component(
            ComponentDef::new("Checkbox", "A checkbox")
                .param(Param::required("label", Type::String))
                .param(Param::required("checked", Type::Boolean).bindable()),
        )
        .component(
            ComponentDef::new("Tabs", "A tab bar; selected is the index of the active tab")
                .param(Param::required("labels", Type::list(Type::String)))
                .param(Param::required("selected", Type::Number).bindable()),
        )
        .component(
            ComponentDef::new("Progress", "A progress bar from 0 to 100")
                .param(Param::required("value", Type::Number)),
        )
        .component(ComponentDef::new("Divider", "A thin separating line"))
        .component(ComponentDef::new(
            "Spacer",
            "Takes the remaining space in a row or column",
        ))
        .component(
            ComponentDef::new("KitStory", "A GPUI Kit gallery story, by title")
                .param(Param::required("name", Type::String)),
        )
        .component(
            ComponentDef::new("AiStory", "A gpui-ai gallery story, by slug")
                .param(Param::required("name", Type::String)),
        )
        .root("Column")
        .build()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use openui::Inputs;

    fn stories(directory: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_dir() {
                stories(&path, found)?;
            } else if path
                .extension()
                .is_some_and(|extension| extension == "gpui-story")
            {
                found.push(path);
            }
        }
        Ok(())
    }

    /// Agents learn the format from these stories, so each must parse and
    /// render without a diagnostic.
    #[test]
    fn every_story_renders_cleanly() -> anyhow::Result<()> {
        let library = super::library()?;
        let mut found = Vec::new();
        stories(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../components/stories"),
            &mut found,
        )?;
        assert!(!found.is_empty());
        for path in found {
            let document = openui::parse(&library, &std::fs::read_to_string(&path)?);
            let evaluation = document.evaluate(&Inputs::new());
            let diagnostics = document
                .diagnostics()
                .iter()
                .chain(evaluation.diagnostics())
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            assert!(
                diagnostics.is_empty(),
                "{}: {diagnostics:?}",
                path.display()
            );
            assert!(
                evaluation.root().is_some(),
                "{} renders nothing",
                path.display()
            );
        }
        Ok(())
    }
}
