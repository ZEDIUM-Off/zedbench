#[cfg(feature = "collab")]
mod audio_input_output_setup;
#[cfg(feature = "collab")]
mod audio_test_window;
#[cfg(feature = "ai")]
mod edit_prediction_provider_setup;
mod external_agents_page;
mod feature_flags;
mod llm_providers_page;
mod mcp_servers_page;
mod sandbox_settings;
mod skill_creator;
mod skills_setup;
#[cfg(feature = "ai")]
mod tool_permissions_setup;

#[cfg(feature = "collab")]
pub(crate) use audio_input_output_setup::{
    render_input_audio_device_dropdown, render_output_audio_device_dropdown,
};
#[cfg(feature = "collab")]
pub(crate) use audio_test_window::open_audio_test_window;
#[cfg(feature = "ai")]
pub(crate) use edit_prediction_provider_setup::render_edit_prediction_setup_page;
pub(crate) use external_agents_page::{
    CustomAgentForm, render_add_agent_popover, render_external_agents_page,
};
pub(crate) use feature_flags::render_feature_flags_page;
pub(crate) use llm_providers_page::{
    LlmProviderForm, render_add_llm_provider_popover, render_llm_providers_page,
};
pub(crate) use mcp_servers_page::{
    McpServerForm, render_add_server_popover, render_mcp_servers_page,
};
pub(crate) use sandbox_settings::render_sandbox_settings_page;
pub use skill_creator::SkillCreatorOpenMode;
pub(crate) use skill_creator::{
    SkillCreatorEvent, SkillCreatorPage, render_skill_creator_page, skill_url_from_clipboard,
};
#[cfg(test)]
pub(crate) use skills_setup::displayed_skills;
pub(crate) use skills_setup::render_skills_setup_page;
#[cfg(test)]
pub(crate) use skills_setup::{copy_skill_share_link, trash_skill};
#[cfg(feature = "ai")]
pub(crate) use tool_permissions_setup::render_tool_permissions_setup_page;

#[cfg(feature = "ai")]
pub use tool_permissions_setup::{
    render_copy_path_tool_config, render_create_directory_tool_config,
    render_delete_path_tool_config, render_edit_file_tool_config, render_fetch_tool_config,
    render_move_path_tool_config, render_skill_tool_config, render_terminal_tool_config,
    render_web_search_tool_config, render_write_file_tool_config,
};

#[cfg(not(feature = "ai"))]
pub(crate) use render_omitted_page as render_edit_prediction_setup_page;
#[cfg(not(feature = "ai"))]
pub(crate) use render_omitted_page as render_tool_permissions_setup_page;

/// Stands in for sub-pages of the AI page, which `settings_data` hides when
/// this build leaves the AI crates out.
#[cfg(not(feature = "ai"))]
pub(crate) fn render_omitted_page(
    _: &crate::SettingsWindow,
    _: &gpui::ScrollHandle,
    _: &mut gpui::Window,
    _: &mut gpui::Context<crate::SettingsWindow>,
) -> gpui::AnyElement {
    gpui::IntoElement::into_any_element(gpui::Empty)
}

/// The Collaboration page is hidden when this build leaves out audio.
#[cfg(not(feature = "collab"))]
pub(crate) fn open_audio_test_window(_: &mut gpui::Window, _: &mut gpui::App) {}
