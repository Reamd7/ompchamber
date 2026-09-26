//! Typed view over the persisted settings object.
//!
//! The write pipeline (`helpers.rs` / `runtime.rs`) keeps settings as a raw
//! `serde_json` map so JS object-spread semantics (undefined-clears, unknown
//! keys, odd-typed values) round-trip byte-faithfully. [`Settings`] is the
//! typed projection over that raw map: it deserializes with defaults for
//! every recognized field and captures everything else in the `extra` map, so
//! serializing a typed view never drops user data.
//!
//! Field set = the fields `settings-helpers.js` actually normalizes/sanitizes
//! (the authoritative list of persisted settings.json keys).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A project entry (`sanitizeProjects` output shape).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub path: String,
    pub label: Option<String>,
    pub icon: Option<String>,
    pub icon_background: Option<String>,
    pub color: Option<String>,
    pub default_model: Option<String>,
    pub default_variant: Option<String>,
    pub added_at: Option<serde_json::Number>,
    pub last_opened_at: Option<serde_json::Number>,
    pub icon_image: Option<IconImage>,
    pub sidebar_collapsed: Option<bool>,
    /// Keys not modeled above survive here.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IconImage {
    pub mime: String,
    pub updated_at: serde_json::Number,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PermissionAutoAccept {
    pub sessions: BTreeMap<String, bool>,
    pub revision: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DraftStarter {
    #[serde(rename = "type")]
    pub starter_type: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SkillCatalog {
    pub id: String,
    pub label: String,
    pub source: String,
    pub subpath: Option<String>,
    pub git_identity_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TunnelPreset {
    pub id: String,
    pub name: String,
    pub hostname: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsageModelGroup {
    pub custom_groups: Vec<CustomModelGroup>,
    pub model_assignments: BTreeMap<String, String>,
    pub renamed_groups: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CustomModelGroup {
    pub id: String,
    pub label: String,
    pub models: Vec<String>,
    pub order: serde_json::Number,
}

macro_rules! settings_fields {
    (
        $( $field:ident : $ty:ty, $key:literal ; )*
    ) => {
        /// Typed projection of persisted settings.json.
        ///
        /// Constructed from the raw persisted map; unknown keys land in
        /// `extra` and are re-emitted on serialize.
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default, rename_all = "camelCase")]
        pub struct Settings {
            $(
                #[serde(rename = $key, skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
            #[serde(flatten)]
            pub extra: Map<String, Value>,
        }

        impl Settings {
            /// Keys of the recognized (typed) fields, in declaration order.
            pub fn known_keys() -> &'static [&'static str] {
                &[$( $key ),*]
            }
        }
    };
}

settings_fields! {
    // --- theme / appearance -------------------------------------------------
    theme_id: String, "themeId";
    theme_variant: String, "themeVariant";
    use_system_theme: bool, "useSystemTheme";
    light_theme_id: String, "lightThemeId";
    dark_theme_id: String, "darkThemeId";
    splash_bg_light: String, "splashBgLight";
    splash_fg_light: String, "splashFgLight";
    splash_bg_dark: String, "splashBgDark";
    splash_fg_dark: String, "splashFgDark";
    // --- directories --------------------------------------------------------
    last_directory: String, "lastDirectory";
    home_directory: String, "homeDirectory";
    // --- work status --------------------------------------------------------
    work_status_panel_enabled: bool, "workStatusPanelEnabled";
    work_status_hidden_sections: Vec<String>, "workStatusHiddenSections";
    // --- desktop ------------------------------------------------------------
    desktop_lan_access_enabled: bool, "desktopLanAccessEnabled";
    desktop_keep_awake_enabled: bool, "desktopKeepAwakeEnabled";
    desktop_minimize_to_tray_enabled: bool, "desktopMinimizeToTrayEnabled";
    desktop_mac_menu_bar_enabled: bool, "desktopMacMenuBarEnabled";
    desktop_window_controls_position: String, "desktopWindowControlsPosition";
    desktop_window_controls_style: String, "desktopWindowControlsStyle";
    desktop_ui_password: String, "desktopUiPassword";
    // --- permissions --------------------------------------------------------
    permission_auto_accept: PermissionAutoAccept, "permissionAutoAccept";
    // --- projects / sidebar -------------------------------------------------
    projects: Vec<Project>, "projects";
    active_project_id: String, "activeProjectId";
    sidebar_project_display_mode: String, "sidebarProjectDisplayMode";
    sidebar_session_grouping_mode: String, "sidebarSessionGroupingMode";
    sidebar_project_sort_order: String, "sidebarProjectSortOrder";
    sidebar_show_recent_section: bool, "sidebarShowRecentSection";
    security_scoped_bookmarks: Vec<String>, "securityScopedBookmarks";
    pinned_directories: Vec<String>, "pinnedDirectories";
    // --- draft starters -----------------------------------------------------
    draft_starters: Vec<DraftStarter>, "draftStarters";
    draft_starters_visible: bool, "draftStartersVisible";
    draft_starters_craft_goal_added: bool, "draftStartersCraftGoalAdded";
    draft_starters_schedule_task_added: bool, "draftStartersScheduleTaskAdded";
    // --- fonts / editor -----------------------------------------------------
    ui_font: String, "uiFont";
    mono_font: String, "monoFont";
    markdown_display_mode: String, "markdownDisplayMode";
    github_client_id: String, "githubClientId";
    github_scopes: String, "githubScopes";
    // --- sessions / rendering ----------------------------------------------
    show_reasoning_traces: bool, "showReasoningTraces";
    session_recap_enabled: bool, "sessionRecapEnabled";
    session_suggestion_enabled: bool, "sessionSuggestionEnabled";
    session_goal_enabled: bool, "sessionGoalEnabled";
    session_goal_default_budget_enabled: bool, "sessionGoalDefaultBudgetEnabled";
    session_goal_default_budget: serde_json::Number, "sessionGoalDefaultBudget";
    collapsible_thinking_blocks: bool, "collapsibleThinkingBlocks";
    show_text_justification_activity: bool, "showTextJustificationActivity";
    show_deletion_dialog: bool, "showDeletionDialog";
    // --- notifications ------------------------------------------------------
    native_notifications_enabled: bool, "nativeNotificationsEnabled";
    notification_mode: String, "notificationMode";
    notify_on_subtasks: bool, "notifyOnSubtasks";
    notify_on_completion: bool, "notifyOnCompletion";
    notify_on_error: bool, "notifyOnError";
    notify_on_question: bool, "notifyOnQuestion";
    notification_templates: Value, "notificationTemplates";
    // --- summaries ----------------------------------------------------------
    summarize_last_message: bool, "summarizeLastMessage";
    summary_threshold: serde_json::Number, "summaryThreshold";
    summary_length: serde_json::Number, "summaryLength";
    max_last_message_length: serde_json::Number, "maxLastMessageLength";
    usage_display_mode: String, "usageDisplayMode";
    usage_dropdown_providers: Vec<String>, "usageDropdownProviders";
    // --- retention ----------------------------------------------------------
    auto_delete_enabled: bool, "autoDeleteEnabled";
    auto_delete_after_days: serde_json::Number, "autoDeleteAfterDays";
    session_retention_action: String, "sessionRetentionAction";
    // --- tunnels ------------------------------------------------------------
    tunnel_bootstrap_ttl_ms: serde_json::Number, "tunnelBootstrapTtlMs";
    tunnel_session_ttl_ms: serde_json::Number, "tunnelSessionTtlMs";
    tunnel_provider: String, "tunnelProvider";
    tunnel_mode: String, "tunnelMode";
    managed_local_tunnel_config_path: String, "managedLocalTunnelConfigPath";
    managed_remote_tunnel_hostname: String, "managedRemoteTunnelHostname";
    managed_remote_tunnel_token: String, "managedRemoteTunnelToken";
    managed_remote_tunnel_presets: Vec<TunnelPreset>, "managedRemoteTunnelPresets";
    managed_remote_tunnel_preset_tokens: BTreeMap<String, String>, "managedRemoteTunnelPresetTokens";
    managed_remote_tunnel_selected_preset_id: String, "managedRemoteTunnelSelectedPresetId";
    // --- typography / models ------------------------------------------------
    typography_sizes: BTreeMap<String, String>, "typographySizes";
    default_model: String, "defaultModel";
    default_variant: String, "defaultVariant";
    default_agent: String, "defaultAgent";
    small_model_use_default: bool, "smallModelUseDefault";
    small_model_override: String, "smallModelOverride";
    walkthrough_model_override: String, "walkthroughModelOverride";
    default_git_identity_id: String, "defaultGitIdentityId";
    follow_up_behavior: String, "followUpBehavior";
    auto_create_worktree: bool, "autoCreateWorktree";
    gitmoji_enabled: bool, "gitmojiEnabled";
    default_file_viewer_preview: bool, "defaultFileViewerPreview";
    zen_model: String, "zenModel";
    git_provider_id: String, "gitProviderId";
    git_model_id: String, "gitModelId";
    pwa_app_name: String, "pwaAppName";
    pwa_orientation: String, "pwaOrientation";
    mobile_keyboard_mode: String, "mobileKeyboardMode";
    // --- chat rendering -----------------------------------------------------
    tool_call_expansion: String, "toolCallExpansion";
    input_spellcheck_enabled: bool, "inputSpellcheckEnabled";
    agent_web_tool_enabled: bool, "agentWebToolEnabled";
    agent_control_tool_enabled: bool, "agentControlToolEnabled";
    agent_memory_tool_enabled: bool, "agentMemoryToolEnabled";
    show_tool_file_icons: bool, "showToolFileIcons";
    show_turn_changed_files: bool, "showTurnChangedFiles";
    show_expanded_bash_tools: bool, "showExpandedBashTools";
    show_expanded_edit_tools: bool, "showExpandedEditTools";
    time_format_preference: String, "timeFormatPreference";
    week_start_preference: String, "weekStartPreference";
    chat_render_mode: String, "chatRenderMode";
    message_stream_transport: String, "messageStreamTransport";
    activity_render_mode: String, "activityRenderMode";
    mermaid_rendering_mode: String, "mermaidRenderingMode";
    user_message_rendering_mode: String, "userMessageRenderingMode";
    collapsible_user_messages: bool, "collapsibleUserMessages";
    sticky_user_header: bool, "stickyUserHeader";
    prompt_navigator_enabled: bool, "promptNavigatorEnabled";
    expanded_editor_toolbar: bool, "expandedEditorToolbar";
    wide_chat_layout_enabled: bool, "wideChatLayoutEnabled";
    show_split_assistant_message_actions: bool, "showSplitAssistantMessageActions";
    // --- sizes --------------------------------------------------------------
    font_size: serde_json::Number, "fontSize";
    terminal_font_size: serde_json::Number, "terminalFontSize";
    editor_font_size: serde_json::Number, "editorFontSize";
    terminal_shell: String, "terminalShell";
    terminal_login_shells: Vec<String>, "terminalLoginShells";
    padding: serde_json::Number, "padding";
    corner_radius: serde_json::Number, "cornerRadius";
    input_bar_offset: serde_json::Number, "inputBarOffset";
    // --- shortcuts / model lists -------------------------------------------
    shortcut_overrides: BTreeMap<String, String>, "shortcutOverrides";
    favorite_models: Vec<ModelRef>, "favoriteModels";
    recent_models: Vec<ModelRef>, "recentModels";
    hidden_models: Vec<ModelRef>, "hiddenModels";
    collapsed_model_providers: Vec<String>, "collapsedModelProviders";
    recent_agents: Vec<String>, "recentAgents";
    recent_efforts: BTreeMap<String, Vec<String>>, "recentEfforts";
    // --- diffs / files ------------------------------------------------------
    diff_layout_preference: String, "diffLayoutPreference";
    git_changes_view_mode: String, "gitChangesViewMode";
    directory_show_hidden: bool, "directoryShowHidden";
    files_view_show_gitignored: bool, "filesViewShowGitignored";
    open_in_app_id: String, "openInAppId";
    message_limit: serde_json::Number, "messageLimit";
    // --- skills / usage -----------------------------------------------------
    skill_catalogs: Vec<SkillCatalog>, "skillCatalogs";
    usage_selected_models: BTreeMap<String, Vec<String>>, "usageSelectedModels";
    usage_collapsed_families: BTreeMap<String, Vec<String>>, "usageCollapsedFamilies";
    usage_expanded_families: BTreeMap<String, Vec<String>>, "usageExpandedFamilies";
    usage_model_groups: BTreeMap<String, UsageModelGroup>, "usageModelGroups";
    report_usage: bool, "reportUsage";
    // --- behavior prompt / response style ----------------------------------
    global_behavior_prompt: String, "globalBehaviorPrompt";
    response_style_enabled: bool, "responseStyleEnabled";
    response_style_preset: String, "responseStylePreset";
    response_style_custom_instructions: String, "responseStyleCustomInstructions";
    // --- dictation / STT ----------------------------------------------------
    dictation_enabled: bool, "dictationEnabled";
    stt_provider: String, "sttProvider";
    stt_server_url: String, "sttServerUrl";
    stt_model: String, "sttModel";
    stt_local_model: String, "sttLocalModel";
    stt_language: String, "sttLanguage";
}

// serde_json::Number has no Default; both structs want derive(Default) for
// their container `#[serde(default)]` semantics.
impl Default for IconImage {
    fn default() -> Self {
        Self {
            mime: String::new(),
            updated_at: serde_json::Number::from(0),
            source: None,
        }
    }
}

impl Default for CustomModelGroup {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            models: Vec::new(),
            order: serde_json::Number::from(0),
        }
    }
}
