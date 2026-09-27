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
//!
//! 中文说明：Settings 是持久化 settings.json 的类型化投影；Option 字段
//! 配合 skip_serializing_if 保证"未设置即不落盘"，extra 承载未识别键，
//! 序列化往返不丢数据。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A project entry (`sanitizeProjects` output shape).
/// 中文说明：字段与 normalization::sanitize_projects 的输出键一一对应；
/// Option 字段缺省时不参与序列化。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Project {
/// 项目唯一标识（`path_<base64url>` 形式）。
    pub id: String,
/// 规范化后的项目绝对路径（realpath 已解析）。
    pub path: String,
/// 可选的用户自定义显示名。
    pub label: Option<String>,
/// 可选 icon 标识。
    pub icon: Option<String>,
/// 可选 icon 背景色（合法 #hex 颜色，清洗后为小写）。
    pub icon_background: Option<String>,
/// 可选颜色标记。
    pub color: Option<String>,
/// 可选默认模型（providerID/modelID 形式）。
    pub default_model: Option<String>,
/// 默认模型变体；仅当 defaultModel 存在时保留。
    pub default_variant: Option<String>,
/// 添加时间戳数值（清洗要求非负有限数）。
    pub added_at: Option<serde_json::Number>,
/// 最近打开时间戳数值（同样要求非负有限数）。
    pub last_opened_at: Option<serde_json::Number>,
/// 可选图标图源信息（mime/updatedAt/source 三字段）。
    pub icon_image: Option<IconImage>,
/// 侧边栏中该项目是否折叠。
    pub sidebar_collapsed: Option<bool>,
    /// Keys not modeled above survive here.
/// 未显式建模的键在此原样保留，序列化时原样回写。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// 项目图标图源信息（sanitizeProjects 仅在 mime 非空、updatedAt>0、
/// source 合法时才保留该对象）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IconImage {
/// MIME 类型（如 image/png）。
    pub mime: String,
/// 更新时间戳数值（清洗时取整并钳制为非负）。
    pub updated_at: serde_json::Number,
/// 来源标记，取值 custom 或 auto。
    pub source: Option<String>,
}

/// 权限自动接受状态（按 session 记录）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PermissionAutoAccept {
/// sessionID → 是否自动接受 的映射。
    pub sessions: BTreeMap<String, bool>,
/// 配置修订号。
    pub revision: i64,
}

/// 草稿 starter 定义。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DraftStarter {
/// starter 类型（serde 键固定为 "type"）。
    #[serde(rename = "type")]
    pub starter_type: String,
/// 显示名称。
    pub name: String,
}

/// 模型引用（收藏/最近/隐藏模型列表的元素）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRef {
/// provider 标识（serde 键 "providerID"）。
    #[serde(rename = "providerID")]
    pub provider_id: String,
/// 模型标识（serde 键 "modelID"）。
    #[serde(rename = "modelID")]
    pub model_id: String,
}

/// skill 仓库目录条目（sanitizeSkillCatalogs 的输出形状）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SkillCatalog {
/// 目录条目标识。
    pub id: String,
/// 显示名。
    pub label: String,
/// 仓库来源字符串。
    pub source: String,
/// 仓库内子目录（可选）。
    pub subpath: Option<String>,
/// 使用的 git identity 标识（可选）。
    pub git_identity_id: Option<String>,
}

/// managed remote tunnel preset。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TunnelPreset {
/// preset 标识。
    pub id: String,
/// 显示名。
    pub name: String,
/// 主机名（已归一为小写）。
    pub hostname: String,
}

/// 用量统计中的模型分组配置。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsageModelGroup {
/// 用户自定义分组列表。
    pub custom_groups: Vec<CustomModelGroup>,
/// 模型到分组的分配映射。
    pub model_assignments: BTreeMap<String, String>,
/// 分组重命名映射。
    pub renamed_groups: BTreeMap<String, String>,
}

/// 单个自定义模型分组。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CustomModelGroup {
/// 分组标识。
    pub id: String,
/// 显示名。
    pub label: String,
/// 分组内的模型引用列表。
    pub models: Vec<String>,
/// 排序权重数值。
    pub order: serde_json::Number,
}

/// 声明式生成 Settings：字段表同时驱动结构体定义与 known_keys()，
/// 保证 JSON 键清单与字段声明永不脱节。
macro_rules! settings_fields {
    (
        $( $field:ident : $ty:ty, $key:literal ; )*
    ) => {
        /// Typed projection of persisted settings.json.
        ///
        /// Constructed from the raw persisted map; unknown keys land in
        /// `extra` and are re-emitted on serialize.
        /// 中文说明：由 settings_fields! 宏展开生成，字段即下方清单。
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
            /// 返回顺序即宏字段的声明顺序，可用于与 JS 键清单对齐校验。
            pub fn known_keys() -> &'static [&'static str] {
                &[$( $key ),*]
            }
        }
    };
}

// 展开持久化 settings.json 的全部已知字段（分区注释标出功能域）。
// （宏调用语句上不能用 ///，会触发 unused_doc_comments。）
settings_fields! {
    // --- theme / appearance -------------------------------------------------
    // 主题 id。
    theme_id: String, "themeId";
    // 主题变体。
    theme_variant: String, "themeVariant";
    // 是否跟随系统主题。
    use_system_theme: bool, "useSystemTheme";
    // 浅色模式使用的主题 id。
    light_theme_id: String, "lightThemeId";
    // 深色模式使用的主题 id。
    dark_theme_id: String, "darkThemeId";
    // 启动页浅色背景色。
    splash_bg_light: String, "splashBgLight";
    // 启动页浅色前景色。
    splash_fg_light: String, "splashFgLight";
    // 启动页深色背景色。
    splash_bg_dark: String, "splashBgDark";
    // 启动页深色前景色。
    splash_fg_dark: String, "splashFgDark";
    // --- directories --------------------------------------------------------
    // 上次打开的目录。
    last_directory: String, "lastDirectory";
    // 用户主目录。
    home_directory: String, "homeDirectory";
    // --- work status --------------------------------------------------------
    // 是否启用工作状态面板。
    work_status_panel_enabled: bool, "workStatusPanelEnabled";
    // 工作状态面板中隐藏的分区列表。
    work_status_hidden_sections: Vec<String>, "workStatusHiddenSections";
    // --- desktop ------------------------------------------------------------
    // 桌面端是否允许局域网访问。
    desktop_lan_access_enabled: bool, "desktopLanAccessEnabled";
    // 桌面端是否阻止系统休眠。
    desktop_keep_awake_enabled: bool, "desktopKeepAwakeEnabled";
    // 桌面端关闭窗口时是否最小化到托盘。
    desktop_minimize_to_tray_enabled: bool, "desktopMinimizeToTrayEnabled";
    // macOS 是否启用菜单栏模式。
    desktop_mac_menu_bar_enabled: bool, "desktopMacMenuBarEnabled";
    // 桌面窗口控制按钮位置。
    desktop_window_controls_position: String, "desktopWindowControlsPosition";
    // 桌面窗口控制按钮样式。
    desktop_window_controls_style: String, "desktopWindowControlsStyle";
    // 桌面端 UI 访问密码。
    desktop_ui_password: String, "desktopUiPassword";
    // --- permissions --------------------------------------------------------
    // 按 session 记录的权限自动接受状态。
    permission_auto_accept: PermissionAutoAccept, "permissionAutoAccept";
    // --- projects / sidebar -------------------------------------------------
    // 项目列表（清洗规则见 normalization::sanitize_projects）。
    projects: Vec<Project>, "projects";
    // 当前激活项目的 id。
    active_project_id: String, "activeProjectId";
    // 侧边栏项目展示模式。
    sidebar_project_display_mode: String, "sidebarProjectDisplayMode";
    // 侧边栏会话分组模式。
    sidebar_session_grouping_mode: String, "sidebarSessionGroupingMode";
    // 侧边栏项目排序方式。
    sidebar_project_sort_order: String, "sidebarProjectSortOrder";
    // 侧边栏是否显示"最近"分区。
    sidebar_show_recent_section: bool, "sidebarShowRecentSection";
    // macOS 安全作用域书签（目录访问授权）列表。
    security_scoped_bookmarks: Vec<String>, "securityScopedBookmarks";
    // 固定（pinned）目录列表，路径已规范化。
    pinned_directories: Vec<String>, "pinnedDirectories";
    // --- draft starters -----------------------------------------------------
    // 草稿 starter 列表。
    draft_starters: Vec<DraftStarter>, "draftStarters";
    // 是否显示 starter 入口。
    draft_starters_visible: bool, "draftStartersVisible";
    // 是否已添加过 craft goal starter。
    draft_starters_craft_goal_added: bool, "draftStartersCraftGoalAdded";
    // 是否已添加过 schedule task starter。
    draft_starters_schedule_task_added: bool, "draftStartersScheduleTaskAdded";
    // --- fonts / editor -----------------------------------------------------
    // UI 字体。
    ui_font: String, "uiFont";
    // 等宽字体。
    mono_font: String, "monoFont";
    // Markdown 渲染模式。
    markdown_display_mode: String, "markdownDisplayMode";
    // GitHub OAuth client id。
    github_client_id: String, "githubClientId";
    // GitHub 授权 scopes。
    github_scopes: String, "githubScopes";
    // --- sessions / rendering ----------------------------------------------
    // 是否显示推理轨迹。
    show_reasoning_traces: bool, "showReasoningTraces";
    // 是否启用会话摘要（recap）。
    session_recap_enabled: bool, "sessionRecapEnabled";
    // 是否启用会话建议。
    session_suggestion_enabled: bool, "sessionSuggestionEnabled";
    // 是否启用会话目标。
    session_goal_enabled: bool, "sessionGoalEnabled";
    // 会话目标是否使用默认预算。
    session_goal_default_budget_enabled: bool, "sessionGoalDefaultBudgetEnabled";
    // 会话目标默认预算数值。
    session_goal_default_budget: serde_json::Number, "sessionGoalDefaultBudget";
    // 思考块是否可折叠。
    collapsible_thinking_blocks: bool, "collapsibleThinkingBlocks";
    // 是否显示文本论证活动。
    show_text_justification_activity: bool, "showTextJustificationActivity";
    // 删除操作是否弹出确认对话框。
    show_deletion_dialog: bool, "showDeletionDialog";
    // --- notifications ------------------------------------------------------
    // 是否启用系统原生通知。
    native_notifications_enabled: bool, "nativeNotificationsEnabled";
    // 通知模式。
    notification_mode: String, "notificationMode";
    // 子任务完成时是否通知。
    notify_on_subtasks: bool, "notifyOnSubtasks";
    // 任务完成时是否通知。
    notify_on_completion: bool, "notifyOnCompletion";
    // 出错时是否通知。
    notify_on_error: bool, "notifyOnError";
    // 收到提问时是否通知。
    notify_on_question: bool, "notifyOnQuestion";
    // 通知模板（原始 JSON，结构由前端定义）。
    notification_templates: Value, "notificationTemplates";
    // --- summaries ----------------------------------------------------------
    // 是否对最后一条消息做摘要。
    summarize_last_message: bool, "summarizeLastMessage";
    // 触发摘要的阈值。
    summary_threshold: serde_json::Number, "summaryThreshold";
    // 摘要长度。
    summary_length: serde_json::Number, "summaryLength";
    // 参与摘要的最后一条消息最大长度。
    max_last_message_length: serde_json::Number, "maxLastMessageLength";
    // 用量展示模式。
    usage_display_mode: String, "usageDisplayMode";
    // 用量下拉中显示的 provider 列表。
    usage_dropdown_providers: Vec<String>, "usageDropdownProviders";
    // --- retention ----------------------------------------------------------
    // 是否启用自动删除。
    auto_delete_enabled: bool, "autoDeleteEnabled";
    // 自动删除的天数阈值。
    auto_delete_after_days: serde_json::Number, "autoDeleteAfterDays";
    // 会话保留策略动作。
    session_retention_action: String, "sessionRetentionAction";
    // --- tunnels ------------------------------------------------------------
    // tunnel bootstrap 轮询 TTL（毫秒），清洗规则见 normalization。
    tunnel_bootstrap_ttl_ms: serde_json::Number, "tunnelBootstrapTtlMs";
    // tunnel 会话 TTL（毫秒）。
    tunnel_session_ttl_ms: serde_json::Number, "tunnelSessionTtlMs";
    // tunnel provider（cloudflare/ngrok）。
    tunnel_provider: String, "tunnelProvider";
    // tunnel 模式（quick/managed-remote/managed-local）。
    tunnel_mode: String, "tunnelMode";
    // managed local tunnel 配置文件路径。
    managed_local_tunnel_config_path: String, "managedLocalTunnelConfigPath";
    // managed remote tunnel 主机名（小写归一）。
    managed_remote_tunnel_hostname: String, "managedRemoteTunnelHostname";
    // managed remote tunnel 访问 token。
    managed_remote_tunnel_token: String, "managedRemoteTunnelToken";
    // managed remote tunnel preset 列表。
    managed_remote_tunnel_presets: Vec<TunnelPreset>, "managedRemoteTunnelPresets";
    // preset id → token 映射。
    managed_remote_tunnel_preset_tokens: BTreeMap<String, String>, "managedRemoteTunnelPresetTokens";
    // 当前选中的 preset id。
    managed_remote_tunnel_selected_preset_id: String, "managedRemoteTunnelSelectedPresetId";
    // --- typography / models ------------------------------------------------
    // 排版尺寸映射（markdown/code/uiHeader 等键）。
    typography_sizes: BTreeMap<String, String>, "typographySizes";
    // 默认模型（providerID/modelID）。
    default_model: String, "defaultModel";
    // 默认模型变体。
    default_variant: String, "defaultVariant";
    // 默认 agent。
    default_agent: String, "defaultAgent";
    // 小模型是否跟随默认模型。
    small_model_use_default: bool, "smallModelUseDefault";
    // 小模型覆盖值。
    small_model_override: String, "smallModelOverride";
    // walkthrough 场景的模型覆盖。
    walkthrough_model_override: String, "walkthroughModelOverride";
    // 默认 git identity id。
    default_git_identity_id: String, "defaultGitIdentityId";
    // 追问（follow-up）行为设置。
    follow_up_behavior: String, "followUpBehavior";
    // 是否自动创建 git worktree。
    auto_create_worktree: bool, "autoCreateWorktree";
    // 提交信息是否启用 gitmoji。
    gitmoji_enabled: bool, "gitmojiEnabled";
    // 文件查看器默认是否预览。
    default_file_viewer_preview: bool, "defaultFileViewerPreview";
    // Zen 模式使用的模型。
    zen_model: String, "zenModel";
    // git 专用 provider id。
    git_provider_id: String, "gitProviderId";
    // git 专用 model id。
    git_model_id: String, "gitModelId";
    // PWA 应用名。
    pwa_app_name: String, "pwaAppName";
    // PWA 屏幕方向。
    pwa_orientation: String, "pwaOrientation";
    // 移动端键盘模式。
    mobile_keyboard_mode: String, "mobileKeyboardMode";
    // --- chat rendering -----------------------------------------------------
    // 工具调用块的展开方式。
    tool_call_expansion: String, "toolCallExpansion";
    // 输入框是否启用拼写检查。
    input_spellcheck_enabled: bool, "inputSpellcheckEnabled";
    // 是否启用 agent 的 web 工具。
    agent_web_tool_enabled: bool, "agentWebToolEnabled";
    // 是否启用 agent 的 control 工具。
    agent_control_tool_enabled: bool, "agentControlToolEnabled";
    // 是否启用 agent 的 memory 工具。
    agent_memory_tool_enabled: bool, "agentMemoryToolEnabled";
    // 工具调用是否显示文件图标。
    show_tool_file_icons: bool, "showToolFileIcons";
    // 是否显示本轮变更的文件。
    show_turn_changed_files: bool, "showTurnChangedFiles";
    // 是否展开显示 bash 工具调用。
    show_expanded_bash_tools: bool, "showExpandedBashTools";
    // 是否展开显示编辑类工具调用。
    show_expanded_edit_tools: bool, "showExpandedEditTools";
    // 时间显示格式偏好。
    time_format_preference: String, "timeFormatPreference";
    // 一周起始日偏好。
    week_start_preference: String, "weekStartPreference";
    // 聊天渲染模式。
    chat_render_mode: String, "chatRenderMode";
    // 消息流传输方式（如 SSE/WebSocket）。
    message_stream_transport: String, "messageStreamTransport";
    // 活动流渲染模式。
    activity_render_mode: String, "activityRenderMode";
    // Mermaid 图渲染模式。
    mermaid_rendering_mode: String, "mermaidRenderingMode";
    // 用户消息渲染模式。
    user_message_rendering_mode: String, "userMessageRenderingMode";
    // 用户消息是否可折叠。
    collapsible_user_messages: bool, "collapsibleUserMessages";
    // 用户消息是否使用吸附式头部。
    sticky_user_header: bool, "stickyUserHeader";
    // 是否启用 prompt 导航器。
    prompt_navigator_enabled: bool, "promptNavigatorEnabled";
    // 是否显示扩展编辑器工具栏。
    expanded_editor_toolbar: bool, "expandedEditorToolbar";
    // 是否启用宽幅聊天布局。
    wide_chat_layout_enabled: bool, "wideChatLayoutEnabled";
    // 是否显示拆分的助手消息操作按钮。
    show_split_assistant_message_actions: bool, "showSplitAssistantMessageActions";
    // --- sizes --------------------------------------------------------------
    // 全局字号。
    font_size: serde_json::Number, "fontSize";
    // 终端字号。
    terminal_font_size: serde_json::Number, "terminalFontSize";
    // 编辑器字号。
    editor_font_size: serde_json::Number, "editorFontSize";
    // 终端默认 shell。
    terminal_shell: String, "terminalShell";
    // 可选的登录 shell 列表。
    terminal_login_shells: Vec<String>, "terminalLoginShells";
    // 界面内边距。
    padding: serde_json::Number, "padding";
    // 圆角半径。
    corner_radius: serde_json::Number, "cornerRadius";
    // 输入栏偏移量。
    input_bar_offset: serde_json::Number, "inputBarOffset";
    // --- shortcuts / model lists -------------------------------------------
    // 快捷键覆盖映射。
    shortcut_overrides: BTreeMap<String, String>, "shortcutOverrides";
    // 收藏的模型引用列表。
    favorite_models: Vec<ModelRef>, "favoriteModels";
    // 最近使用的模型引用列表。
    recent_models: Vec<ModelRef>, "recentModels";
    // 已隐藏的模型引用列表。
    hidden_models: Vec<ModelRef>, "hiddenModels";
    // 模型选择器中折叠的 provider 列表。
    collapsed_model_providers: Vec<String>, "collapsedModelProviders";
    // 最近使用的 agent 列表。
    recent_agents: Vec<String>, "recentAgents";
    // 最近使用的 effort 选择，按键分组。
    recent_efforts: BTreeMap<String, Vec<String>>, "recentEfforts";
    // --- diffs / files ------------------------------------------------------
    // diff 视图布局偏好。
    diff_layout_preference: String, "diffLayoutPreference";
    // git 变更视图模式。
    git_changes_view_mode: String, "gitChangesViewMode";
    // 文件树是否显示隐藏文件。
    directory_show_hidden: bool, "directoryShowHidden";
    // 文件视图是否显示 gitignored 文件。
    files_view_show_gitignored: bool, "filesViewShowGitignored";
    // "在应用中打开"的目标应用 id。
    open_in_app_id: String, "openInAppId";
    // 消息加载数量上限。
    message_limit: serde_json::Number, "messageLimit";
    // --- skills / usage -----------------------------------------------------
    // skill 仓库目录列表。
    skill_catalogs: Vec<SkillCatalog>, "skillCatalogs";
    // 用量统计选中的模型集合，按键分组。
    usage_selected_models: BTreeMap<String, Vec<String>>, "usageSelectedModels";
    // 用量视图中折叠的模型族列表。
    usage_collapsed_families: BTreeMap<String, Vec<String>>, "usageCollapsedFamilies";
    // 用量视图中展开的模型族列表。
    usage_expanded_families: BTreeMap<String, Vec<String>>, "usageExpandedFamilies";
    // 用量模型分组定义。
    usage_model_groups: BTreeMap<String, UsageModelGroup>, "usageModelGroups";
    // 是否上报使用数据。
    report_usage: bool, "reportUsage";
    // --- behavior prompt / response style ----------------------------------
    // 全局行为提示词。
    global_behavior_prompt: String, "globalBehaviorPrompt";
    // 是否启用回复风格设置。
    response_style_enabled: bool, "responseStyleEnabled";
    // 回复风格预设。
    response_style_preset: String, "responseStylePreset";
    // 回复风格的自定义指令。
    response_style_custom_instructions: String, "responseStyleCustomInstructions";
    // --- dictation / STT ----------------------------------------------------
    // 是否启用语音听写。
    dictation_enabled: bool, "dictationEnabled";
    // 语音识别 provider。
    stt_provider: String, "sttProvider";
    // 自定义 STT 服务地址。
    stt_server_url: String, "sttServerUrl";
    // STT 模型。
    stt_model: String, "sttModel";
    // 本地 STT 模型。
    stt_local_model: String, "sttLocalModel";
    // STT 语言。
    stt_language: String, "sttLanguage";
}

// serde_json::Number has no Default; both structs want derive(Default) for
// their container `#[serde(default)]` semantics.
/// serde_json::Number 没有 Default，手写实现把数值字段初始化为 0，
/// 支撑容器级 #[serde(default)] 语义。
impl Default for IconImage {
    /// 全零值默认：空 mime、0 时间戳、无来源。
    fn default() -> Self {
        Self {
            mime: String::new(),
            updated_at: serde_json::Number::from(0),
            source: None,
        }
    }
}

/// 同 IconImage：手写 Default 使 order 初始化为 0。
impl Default for CustomModelGroup {
    /// 全零值默认：空标识、空模型列表、order 为 0。
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            models: Vec::new(),
            order: serde_json::Number::from(0),
        }
    }
}
