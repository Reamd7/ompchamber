# server-rs 架构文档

> `packages/web/server-rs` — OpenChamber 服务端的 Rust 移植(JS 参照:`packages/web/server/lib`)。
> 本文档按"结构体 + endpoint"刻画整个 crate:组合根、54 个模块、283 个 HTTP 端点、状态所有权与横切语义。
> 模块内逐函数说明见各模块双语文档注释(源内 rustdoc)。

## 1. 定位与边界

- **wire 契约**:实现 `packages/ui/src/lib/opencode/wire` 的 OpenCode 兼容协议;引擎能力经 `EngineState` 代理到 omp host(内嵌 `@oh-my-pi/pi-coding-agent`),OpenChamber 自有能力走本 crate 的 `/api/*` 路由。
- **宿主形态**:CLI(对应 `packages/web/bin`)、Electron 子进程(Rust server 经 desktop-control 私有 socket 回连桌面壳)、隧道/远程会话。
- **平台**:Windows/Linux/macOS;`os_compat` 承接 `std::os::unix` 差异(Windows 为 JS 等价 no-op shim)。

## 2. 组合根与请求生命周期

```rust
// context.rs — 每个模块 router 的唯一注入物
pub struct RouterContext {
    pub config: Arc<ServerConfig>,  // config.rs:端口/数据目录/引擎/隧道选项
    pub engine: Arc<EngineState>,   // engine.rs:reqwest 客户端 + EngineModeKind + RwLock<Option<base_url>>
    pub hub:     Arc<EventHub>,     // hub.rs:SSE 事件中枢(HubEvent 广播)
}
```

- 装配点:`cli/mod.rs` — 37 个子 router 顺序 `.merge()`(ui_auth → proxy → core_routes → settings → fs_routes(带 FsState)→ … → scheduled_tasks),外加 CORS 与 body limits。
- 门控:`core_routes::router` 分两层 — status 路由(`/health`、`/api/version`、`/api/system/*`)不过鉴权;gated 路由(`/api/opencode/*`、`/api/config/*`)套 `ui_auth::middleware`(未配置 UI 密码时直通)。
- 生命周期:请求 → CORS/limit → 路由匹配 → `State<RouterContext>`(或模块私有 State)→ 语义层 → 引擎代理 / 本地服务 → `EventHub` 广播 → SSE。

## 3. 模块总表(54 个 `pub mod`,按域分组)

| 域 | 模块 | 角色 |
|---|---|---|
| 入口/壳 | cli, config, context, error, os_compat, static_assets, pwa_manifest | 子命令分发、选项解析、共享上下文、错误形态、平台 shim、静态资源、PWA 清单 |
| 引擎边界 | engine, engine_env, omp_host_natives, proxy, realtime_proxy, event_stream, relay | 引擎生命周期与环境、请求代理、SSE 事件流、私有中继 |
| 设置/项目 | settings, core_routes, projects, project_context, opencode_routes, opencode_meta, opencode_plugins | settings 存储(读侧迁移自愈)、系统/配置路由、项目/目录解析、图标、skills/plugins |
| 文件系统 | fs_routes, markdown_image_grants, path_realpath_cache | /api/fs 全家 + workspace 准入 + 外授 grant + realpath 缓存 |
| Git/集成 | git_service, github, linear, quota, small_model | git 服务、GitHub PR/审阅、Linear、配额(20+ provider)、小模型 |
| 会话/智能 | openchamber_sessions, openchamber_control, session_folders, session_goal, session_assist, agent_memory, agent_tool, magic_prompts | 会话创建/目录验证、控制面投影、目标/辅助、agent 记忆与工具、提示词 |
| 交互面 | terminal, browser_control, dictation_tts, notifications, dev_servers, walkthrough, skills_catalog | 终端 WS、应用内浏览器代理、语音、推送、dev-server 发现、走查生成、技能目录 |
| 网络/安全 | ui_auth, client_auth, tunnels, desktop_control, permission_auto_accept, package_manager, inherited_env, provider_env_aliases | 鉴权门、远程客户端配对、隧道、桌面控制通道、权限自动接受、包管理、继承环境 |

## 4. 状态所有权

| 结构体 | 所在 | 职责 |
|---|---|---|
| `RouterContext` | context.rs | 三件套注入(config/engine/hub) |
| `EngineState` / `LaunchSpec` / `EngineModeKind` | engine.rs | 引擎 HTTP 客户端、受管/外部模式、base_url 门控 |
| `EventHub` / `HubEvent` | hub.rs | 事件广播(SSE 源) |
| `SettingsStore` | settings/runtime.rs | settings.json 读侧迁移 + 自愈持久化(单一路径语义 owner) |
| `FsState` / `FsInner` / `OutsideGrantStore` / `RealpathCache` | fs_routes | fs 路由状态、工作区准入、外授 grant、realpath 缓存 |
| `GithubState` / `LinearState` / `MetaState` / `NotificationsState` / `EventStreamState` / `ClientAuthState` | 各模块 | 集成/元数据/通知/SSE/配对的模块私有态 |
| `AgentToolRuntime` 等 | agent_tool | agent 自定义工具运行时(含 AbortHandle/ToolResult) |
| `DevServerScanner` / `PromptState` / `GateLayer` | dev_servers / magic_prompts / ui_auth | 发现扫描、提示词状态、鉴权层 |
| `DesktopRuntimeConfig` | realtime_proxy.rs | 桌面实时代理配置 |

## 5. 横切语义(单一 owner)

- **路径归一/自愈**:`settings::normalization` 是唯一 owner —— `normalize_directory_path`(verbatim 剥离 + 重复盘符折叠 + `~`)、`path_resolve`、`safe_realpath`、`strip_verbatim_prefix`、`create_project_id_from_path`。各模块(fs_routes、git_service、opencode_plugins、projects 等)以再导出/委托复用;不得再建副本。
- **Windows 真路径形态**:所有 canonicalize 出口都过 `strip_verbatim_prefix`(对齐 Node `realpathSync`);请求参数与持久化两侧走同一条自愈管线。
- **进程管理**:unix 用信号;Windows 用 tasklist/taskkill(`cli/process.rs` 与 engine.rs 注释互引)。

## 6. 完整端点清单(自动提取自 `.route()` 注册)

### agent_memory/routes.rs (3)

```
GET    /api/agent-memory                                  `get_memory`
GET    /api/agent-memory/all                              `get_all_memory`
PATCH  /api/agent-memory/{memoryId}                       `patch_memory`
```

### browser_control/routes.rs (1)

```
GET    /api/browser-control/result                        `express_404_get`
```

### core_routes/mod.rs (12)

```
GET    /api/config/opencode-resolution                    `resolution`
GET    /api/config/themes                                 `settings_utility`
GET    /api/opencode/health                               `engine_info`
GET    /api/opencode/version                              `engine_info`
GET    /api/system/free-port                              `free_port`
GET    /api/system/info                                   `system_info`
GET    /api/version                                       `api_version`
GET    /health                                            `health`
POST   /api/config/reload                                 `settings_utility`
POST   /api/opencode/directory                            `directory`
POST   /api/system/dev-shutdown                           `dev_shutdown`
POST   /api/system/shutdown                               `system_shutdown`
```

### dev_servers/mod.rs (1)

```
GET    /api/dev-servers                                   `list_dev_servers`
```

### dictation_tts/dictation/mod.rs (5)

```
DELETE /api/dictation/models/{modelId}                    `dictation_model_delete`
GET    /api/dictation/status                              `dictation_status`
GET    /api/dictation/ws                                  `dictation_ws`
POST   /api/dictation/models/{modelId}/download           `dictation_model_download`
POST   /api/dictation/tts/speak                           `dictation_tts_speak`
```

### dictation_tts/tts/mod.rs (7)

```
GET    /api/tts/say/status                                `say_status`
GET    /api/tts/status                                    `tts_status`
POST   /api/stt/transcribe                                `stt_transcribe`
POST   /api/text/summarize                                `text_summarize`
POST   /api/tts/say/speak                                 `say_speak`
POST   /api/tts/speak                                     `tts_speak`
POST   /api/voice/token                                   `voice_token`
```

### fs_routes/mod.rs (16)

```
GET    /api/fs/exec/{jobId}                               `handlers`
GET    /api/fs/git-dirs                                   `handlers`
GET    /api/fs/home                                       `handlers`
GET    /api/fs/list                                       `handlers`
GET    /api/fs/raw                                        `handlers`
GET    /api/fs/read                                       `handlers`
GET    /api/fs/serve/{*path}                              `handlers`
GET    /api/fs/stat                                       `handlers`
POST   /api/fs/clone                                      `handlers`
POST   /api/fs/delete                                     `handlers`
POST   /api/fs/exec                                       `handlers`
POST   /api/fs/mkdir                                      `handlers`
POST   /api/fs/rename                                     `handlers`
POST   /api/fs/reveal                                     `handlers`
POST   /api/fs/upload                                     `handlers`
POST   /api/fs/write                                      `handlers`
```

### git_service/routes.rs (65)

```
DELETE /api/git/remote-branches                           `remote_branches_delete`
GET    /api/git/branch-base                               `branch_base`
GET    /api/git/branches                                  `branches_list`
GET    /api/git/check                                     `git_check`
GET    /api/git/commit-file-diff                          `commit_file_diff`
GET    /api/git/commit-files                              `commit_files`
GET    /api/git/conflict-details                          `conflict_details`
GET    /api/git/current-identity                          `current_identity`
GET    /api/git/diff                                      `diff`
GET    /api/git/discover-credentials                      `discover_credentials`
GET    /api/git/file-diff                                 `file_diff`
GET    /api/git/global-identity                           `global_identity`
GET    /api/git/has-local-identity                        `has_local_identity`
GET    /api/git/identities                                `identities_list`
GET    /api/git/log                                       `log_route`
GET    /api/git/primary-root                              `primary_root`
GET    /api/git/range-diff                                `range_diff`
GET    /api/git/range-files                               `range_files`
GET    /api/git/remote-url                                `remote_url`
GET    /api/git/remotes                                   `remotes_list`
GET    /api/git/stashes                                   `stashes_list`
GET    /api/git/status                                    `status`
GET    /api/git/toplevel                                  `toplevel`
GET    /api/git/worktree-type                             `worktree_type`
GET    /api/git/worktrees                                 `worktrees_list`
GET    /api/git/worktrees/bootstrap-status                `worktrees_bootstrap_status`
POST   /api/git/apply-hunk                                `apply_hunk`
POST   /api/git/branch-push-status                        `branch_push_status`
POST   /api/git/canonicalize-worktree-state               `canonicalize_worktree_state_route`
POST   /api/git/checkout                                  `checkout_route`
POST   /api/git/checkout-commit                           `checkout_commit`
POST   /api/git/cherry-pick                               `cherry_pick_route`
POST   /api/git/commit                                    `commit_route`
POST   /api/git/commit-summaries                          `commit_summaries`
POST   /api/git/fetch                                     `fetch_route`
POST   /api/git/integrate/abort                           `integrate_abort`
POST   /api/git/integrate/cherry-pick-status              `integrate_cherry_pick_status`
POST   /api/git/integrate/conflict-details                `integrate_conflict_details`
POST   /api/git/integrate/continue                        `integrate_continue`
POST   /api/git/integrate/plan                            `integrate_plan`
POST   /api/git/integrate/run                             `integrate_run`
POST   /api/git/merge                                     `merge_route`
POST   /api/git/merge/abort                               `merge_abort`
POST   /api/git/merge/continue                            `merge_continue`
POST   /api/git/pull                                      `pull`
POST   /api/git/push                                      `push`
POST   /api/git/rebase                                    `rebase_route`
POST   /api/git/rebase/abort                              `rebase_abort`
POST   /api/git/rebase/continue                           `rebase_continue`
POST   /api/git/reset-to-commit                           `reset_to_commit_route`
POST   /api/git/revert                                    `revert`
POST   /api/git/revert-commit                             `revert_commit_route`
POST   /api/git/set-identity                              `set_identity`
POST   /api/git/stage                                     `stage`
POST   /api/git/stash                                     `stash_push_route`
POST   /api/git/stash/apply                               `stash_apply_route`
POST   /api/git/stash/drop                                `stash_drop_route`
POST   /api/git/stash/pop                                 `stash_pop_route`
POST   /api/git/stashes/file-counts                       `stashes_file_counts`
POST   /api/git/unstage                                   `unstage`
POST   /api/git/validate-directory                        `validate_directory_route`
POST   /api/git/worktrees/preview                         `worktrees_preview`
POST   /api/git/worktrees/validate                        `worktrees_validate`
PUT    /api/git/branches/rename                           `branches_rename`
PUT    /api/git/identities/{id}                           `identities_update`
```

### github/mod.rs (38)

```
DELETE /api/github/auth                                   `auth_delete`
DELETE /api/github/auth                                   `auth_delete`
GET    /api/github/auth/status                            `auth_status`
GET    /api/github/auth/status                            `auth_status`
GET    /api/github/issues/comments                        `issues_comments`
GET    /api/github/issues/comments                        `issues_comments`
GET    /api/github/issues/get                             `issues_get`
GET    /api/github/issues/get                             `issues_get`
GET    /api/github/issues/list                            `issues_list`
GET    /api/github/issues/list                            `issues_list`
GET    /api/github/me                                     `me`
GET    /api/github/me                                     `me`
GET    /api/github/pr/status                              `pr_status_route`
GET    /api/github/pr/status                              `pr_status_route`
GET    /api/github/pulls/context                          `pulls_context`
GET    /api/github/pulls/context                          `pulls_context`
GET    /api/github/pulls/list                             `pulls_list`
GET    /api/github/pulls/list                             `pulls_list`
GET    /api/github/repo/branches                          `repo_branches`
GET    /api/github/repo/branches                          `repo_branches`
GET    /api/github/repo/upstream                          `repo_upstream`
GET    /api/github/repo/upstream                          `repo_upstream`
POST   /api/github/auth/activate                          `auth_activate`
POST   /api/github/auth/activate                          `auth_activate`
POST   /api/github/auth/complete                          `auth_complete`
POST   /api/github/auth/complete                          `auth_complete`
POST   /api/github/auth/gh-cli                            `auth_gh_cli`
POST   /api/github/auth/gh-cli                            `auth_gh_cli`
POST   /api/github/auth/start                             `auth_start`
POST   /api/github/auth/start                             `auth_start`
POST   /api/github/pr/create                              `pr_create`
POST   /api/github/pr/create                              `pr_create`
POST   /api/github/pr/merge                               `pr_merge`
POST   /api/github/pr/merge                               `pr_merge`
POST   /api/github/pr/ready                               `pr_ready`
POST   /api/github/pr/ready                               `pr_ready`
POST   /api/github/pr/update                              `pr_update`
POST   /api/github/pr/update                              `pr_update`
```

### linear/routes.rs (12)

```
DELETE /api/linear/auth                                   `auth_delete`
GET    /api/linear/auth/status                            `auth_status`
GET    /api/linear/issues/get                             `issues_get`
GET    /api/linear/issues/list                            `issues_list`
GET    /api/linear/issues/states                          `issues_states`
GET    /api/linear/mapping                                `mapping_get`
GET    /api/linear/preferences                            `preferences_get`
GET    /linear/oauth/callback                             `oauth_callback`
POST   /api/linear/auth/activate                          `auth_activate`
POST   /api/linear/auth/start                             `auth_start`
POST   /api/linear/issues/update                          `issues_update`
POST   /api/linear/session-status                         `session_status`
```

### magic_prompts/mod.rs (2)

```
GET    /api/magic-prompts                                 `get_prompts`
GET    /api/magic-prompts                                 `get_prompts`
```

### markdown_image_grants/mod.rs (1)

```
POST   /api/ompchamber/sessions/{sessionId}/markdown-image-grants `mint_grants`
```

### notifications/routes.rs (18)

```
DELETE /api/push/apns-token                               `apns_token_delete`
DELETE /api/push/subscribe                                `push_unsubscribe`
GET    /api/notifications/stream                          `notifications_stream`
GET    /api/push/vapid-public-key                         `vapid_public_key`
GET    /api/push/visibility                               `push_visibility_get`
GET    /api/session-activity                              `session_activity`
GET    /api/sessions/attention                            `sessions_attention`
GET    /api/sessions/snapshot                             `sessions_snapshot`
GET    /api/sessions/status                               `sessions_status`
GET    /api/sessions/{id}/attention                       `session_attention`
GET    /api/sessions/{id}/status                          `session_status`
POST   /api/notifications/auto-accept                     `auto_accept`
POST   /api/push/apns-token                               `apns_token`
POST   /api/push/subscribe                                `push_subscribe`
POST   /api/push/visibility                               `push_visibility`
POST   /api/sessions/{id}/message-sent                    `session_message_sent`
POST   /api/sessions/{id}/unview                          `session_unview`
POST   /api/sessions/{id}/view                            `session_view`
```

### openchamber_control/routes.rs (1)

```
POST   /api/ompchamber/control                            `control_action`
```

### openchamber_sessions/routes.rs (3)

```
POST   /api/ompchamber/sessions                           `create_route`
POST   /api/ompchamber/sessions/{sessionId}/fork          `fork_route`
POST   /api/ompchamber/sessions/{sessionId}/send          `send_route`
```

### opencode_meta/openchamber_routes.rs (4)

```
GET    /api/ompchamber/models-metadata                    `models_metadata`
GET    /api/ompchamber/update-check                       `update_check`
GET    /api/zen/models                                    `zen_models`
POST   /api/ompchamber/update-install                     `update_install`
```

### opencode_meta/project_icon_routes.rs (2)

```
GET    /api/projects/{projectId}/icon                     `get_icon`
POST   /api/projects/{projectId}/icon/discover            `discover_icon`
```

### opencode_plugins/plugin_routes.rs (6)

```
GET    /api/config/plugins                                `list_plugins`
GET    /api/config/plugins/entry/{id}                     `get_entry`
GET    /api/config/plugins/file/{id}                      `get_file`
GET    /api/config/plugins/registry                       `registry_status`
POST   /api/config/plugins/entry                          `post_entry`
POST   /api/config/plugins/file                           `post_file`
```

### opencode_plugins/skill_routes.rs (8)

```
GET    /api/config/skills                                 `list_skills`
GET    /api/config/skills/catalog                         `catalog_list`
GET    /api/config/skills/catalog/source                  `catalog_source`
GET    /api/config/skills/{name}                          `get_skill`
GET    /api/config/skills/{name}/files/{*file_path}       `get_skill_file`
GET    /skill                                             `move`
POST   /api/config/skills/install                         `install_repo`
POST   /api/config/skills/scan                            `scan_repo`
```

### opencode_routes/entity_routes.rs (5)

```
GET    /api/config/agents/{name}                          `get_agent_sources_route`
GET    /api/config/agents/{name}/config                   `get_agent_config_route`
GET    /api/config/commands/{name}                        `get_command_sources_route`
GET    /api/config/mcp                                    `list_mcp_route`
GET    /api/config/mcp/{name}                             `get_mcp_route`
```

### opencode_routes/routes.rs (6)

```
DELETE /api/provider/{providerId}/auth                    `delete_provider_auth`
GET    /api/behavior/agents-md                            `get_behavior_agents_md`
GET    /api/provider/{providerId}/source                  `provider_source`
GET    /mcp/oauth/callback                                `mcp_oauth_callback`
POST   /api/mcp/auth/pending                              `store_pending_mcp_auth`
PUT    /api/provider                                      `put_provider`
```

### permission_auto_accept/mod.rs (4)

```
GET    /api/permission-auto-accept                        `get_policy`
GET    /api/permission-auto-accept                        `get_policy`
PUT    /api/permission-auto-accept/sessions/{sessionId}   `put_session`
PUT    /api/permission-auto-accept/sessions/{sessionId}   `put_session`
```

### project_context/routes.rs (6)

```
GET    /api/project-context/{projectId}                   `get_context`
PATCH  /api/project-context/{projectId}/notes/{noteId}    `patch_note`
PATCH  /api/project-context/{projectId}/plans/{planId}    `patch_plan_pinned`
POST   /api/project-context/{projectId}/notes             `post_notes`
POST   /api/project-context/{projectId}/plans             `post_plans`
PUT    /api/project-context/{projectId}/todos             `put_todos`
```

### proxy/mod.rs (2)

```
GET    /api/event                                         `sse_handler`
GET    /api/global/event                                  `sse_handler`
```

### pwa_manifest/mod.rs (1)

```
GET    /manifest.webmanifest                              `manifest`
```

### quota/routes.rs (5)

```
GET    /api/quota/credentials/{providerId}                `credential_status`
GET    /api/quota/providers                               `list_providers`
GET    /api/quota/{providerId}                            `provider_quota`
POST   /api/quota/credentials/{providerId}/import         `import_credential`
POST   /api/quota/credentials/{providerId}/validate       `validate_credential`
```

### relay/routes.rs (2)

```
GET    /api/ompchamber/relay/status                       `status`
POST   /api/ompchamber/relay/disable                      `disable`
```

### scheduled_tasks/routes.rs (9)

```
GET    /api/ompchamber/events                             `events`
GET    /api/ompchamber/scheduled-tasks/status             `status`
GET    /api/ompchamber/scheduled-tasks/status             `status`
GET    /api/projects/{projectId}/scheduled-tasks          `list_tasks`
GET    /api/projects/{projectId}/scheduled-tasks          `list_tasks`
PATCH  /api/projects/{projectId}/scheduled-tasks/{taskId}/loop-file `set_loop_enabled`
PATCH  /api/projects/{projectId}/scheduled-tasks/{taskId}/loop-file `set_loop_enabled`
POST   /api/projects/{projectId}/scheduled-tasks/{taskId}/run `run_task`
POST   /api/projects/{projectId}/scheduled-tasks/{taskId}/run `run_task`
```

### session_folders/mod.rs (2)

```
GET    /api/session-folders                               `get_folders`
GET    /api/session-folders                               `get_folders`
```

### session_goal/routes.rs (1)

```
PUT    /api/goals/objective/{sessionId}                   `put_objective`
```

### settings/routes.rs (1)

```
GET    /api/config/settings                               `get_settings`
```

### small_model/routes.rs (2)

```
GET    /api/small-model                                   `describe`
POST   /api/small-model/generate                          `generate`
```

### terminal/mod.rs (10)

```
DELETE /api/terminal/{sessionId}                          `close_terminal`
GET    /api/terminal/sessions                             `list_sessions`
GET    /api/terminal/shells                               `list_shells`
GET    /api/terminal/ws                                   `terminal_ws`
POST   /api/terminal/create                               `create_terminal`
POST   /api/terminal/force-kill                           `force_kill`
POST   /api/terminal/touch                                `touch_sessions`
POST   /api/terminal/{sessionId}/appearance               `update_appearance`
POST   /api/terminal/{sessionId}/resize                   `resize_terminal`
POST   /api/terminal/{sessionId}/restart                  `restart_terminal`
```

### tunnels/routes.rs (8)

```
GET    /api/dev-tunnel                                    `dev_tunnel_ws`
GET    /api/ompchamber/tunnel/check                       `tunnel_check`
GET    /api/ompchamber/tunnel/doctor                      `handle_tunnel_doctor`
GET    /api/ompchamber/tunnel/providers                   `tunnel_providers`
GET    /api/ompchamber/tunnel/status                      `tunnel_status`
POST   /api/ompchamber/tunnel/start                       `post_tunnel_start`
POST   /api/ompchamber/tunnel/stop                        `post_tunnel_stop`
PUT    /api/ompchamber/tunnel/managed-remote-token        `put_managed_remote_token`
```

### ui_auth/mod.rs (10)

```
DELETE /api/passkeys/{id}                                 `passkey_revoke`
GET    /api/passkeys                                      `passkey_list`
GET    /auth/passkey/status                               `passkey_status`
GET    /auth/session                                      `session_status`
POST   /api/auth/reset                                    `reset_auth`
POST   /auth/passkey/authenticate/options                 `passkey_auth_options`
POST   /auth/passkey/authenticate/verify                  `passkey_auth_verify`
POST   /auth/passkey/register/options                     `passkey_register_options`
POST   /auth/passkey/register/verify                      `passkey_register_verify`
POST   /auth/url-token                                    `url_token`
```

### walkthrough/routes.rs (4)

```
GET    /api/walkthrough                                   `get_walkthrough`
GET    /api/walkthrough/progress                          `progress`
POST   /api/walkthrough/cancel                            `cancel`
POST   /api/walkthrough/generate                          `generate`
```

## 7. 数据目录

`~/.config/ompchamber/`(受 `OMPCHAMBER_DATA_DIR` 覆盖):`settings.json`、`projects/path_<base64url>.json`(项目域存储)、`sessions-directories.json`、`run/desktop-control.json`(桌面通道发现)。

## 8. 测试与平台

- 库内单测 + 路由级集成测试;Windows 上 settings/fs/sessions/engine_env 等路径相关套件全绿(含自愈回归)。
- 已知缺口(非路径域、预存量):cli 生命周期、git watcher、proxy、opencode_meta、package_manager、skills_catalog 等套件的 posix fixture 未平台化(约 49 个),提交历史有归档。
