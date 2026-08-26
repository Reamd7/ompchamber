/*
 * Vendored from @opencode-ai/sdk@1.18.18 (MIT License, Copyright OpenCode contributors).
 * This is OMPChamber's owned wire contract: the omp host (packages/web/server/lib/omp-host,
 * engine @oh-my-pi/pi-coding-agent) implements this API surface. Generated code is
 * unmodified except for this header and the error-interceptor import path.
 */
import { type Client, type Options as Options2, type TDataShape } from "./client/index.js";
import type { AgentPartInput, AppAgentsErrors, AppAgentsResponses, AppLogErrors, AppLogResponses, AppSkillsErrors, AppSkillsResponses, Auth as Auth3, AuthRemoveErrors, AuthRemoveResponses, AuthSetErrors, AuthSetResponses, CommandListErrors, CommandListResponses, Config as Config3, ConfigGetErrors, ConfigGetResponses, ConfigProvidersErrors, ConfigProvidersResponses, ConfigUpdateErrors, ConfigUpdateResponses, EventSubscribeResponses, EventTuiCommandExecute, EventTuiPromptAppend, EventTuiSessionSelect, EventTuiToastShow, ExperimentalCapabilitiesGetErrors, ExperimentalCapabilitiesGetResponses, ExperimentalConsoleGetErrors, ExperimentalConsoleGetResponses, ExperimentalConsoleListOrgsErrors, ExperimentalConsoleListOrgsResponses, ExperimentalConsoleSwitchOrgResponses, ExperimentalControlPlaneMoveSessionErrors, ExperimentalControlPlaneMoveSessionResponses, ExperimentalProjectCopyGenerateNameErrors, ExperimentalProjectCopyGenerateNameResponses, ExperimentalResourceListErrors, ExperimentalResourceListResponses, ExperimentalSessionBackgroundErrors, ExperimentalSessionBackgroundResponses, ExperimentalSessionListErrors, ExperimentalSessionListResponses, ExperimentalWorkspaceAdapterListErrors, ExperimentalWorkspaceAdapterListResponses, ExperimentalWorkspaceCreateErrors, ExperimentalWorkspaceCreateResponses, ExperimentalWorkspaceListErrors, ExperimentalWorkspaceListResponses, ExperimentalWorkspaceRemoveErrors, ExperimentalWorkspaceRemoveResponses, ExperimentalWorkspaceStatusErrors, ExperimentalWorkspaceStatusResponses, ExperimentalWorkspaceSyncListErrors, ExperimentalWorkspaceSyncListResponses, ExperimentalWorkspaceWarpErrors, ExperimentalWorkspaceWarpResponses, FileListErrors, FileListResponses, FilePartInput, FilePartSource, FileReadErrors, FileReadResponses, FileStatusErrors, FileStatusResponses, FindFilesErrors, FindFilesResponses, FindSymbolsErrors, FindSymbolsResponses, FindTextErrors, FindTextResponses, FormatterStatusErrors, FormatterStatusResponses, GlobalConfigGetErrors, GlobalConfigGetResponses, GlobalConfigUpdateErrors, GlobalConfigUpdateResponses, GlobalDisposeErrors, GlobalDisposeResponses, GlobalEventResponses, GlobalHealthErrors, GlobalHealthResponses, GlobalUpgradeErrors, GlobalUpgradeResponses, InstanceDisposeErrors, InstanceDisposeResponses, LocationRef, LspStatusErrors, LspStatusResponses, McpAddErrors, McpAddResponses, McpAuthAuthenticateErrors, McpAuthAuthenticateResponses, McpAuthCallbackErrors, McpAuthCallbackResponses, McpAuthRemoveErrors, McpAuthRemoveResponses, McpAuthStartErrors, McpAuthStartResponses, McpConnectErrors, McpConnectResponses, McpDisconnectErrors, McpDisconnectResponses, McpLocalConfig, McpRemoteConfig, McpStatusErrors, McpStatusResponses, ModelRef, MoveSessionDestination, OutputFormat, Part as Part2, PartDeleteErrors, PartDeleteResponses, PartUpdateErrors, PartUpdateResponses, PathGetErrors, PathGetResponses, PermissionListErrors, PermissionListResponses, PermissionReplyErrors, PermissionReplyResponses, PermissionRespondErrors, PermissionRespondResponses, PermissionRuleset, PermissionV2Reply, PermissionV2Source, ProjectCommands, ProjectCurrentErrors, ProjectCurrentResponses, ProjectDirectoriesErrors, ProjectDirectoriesResponses, ProjectIcon, ProjectInitGitErrors, ProjectInitGitResponses, ProjectListErrors, ProjectListResponses, ProjectUpdateErrors, ProjectUpdateResponses, PromptInput, ProviderAuthErrors, ProviderAuthResponses, ProviderListErrors, ProviderListResponses, ProviderOauthAuthorizeErrors, ProviderOauthAuthorizeResponses, ProviderOauthCallbackErrors, ProviderOauthCallbackResponses, PtyConnectErrors, PtyConnectResponses, PtyConnectTokenErrors, PtyConnectTokenResponses, PtyCreateErrors, PtyCreateResponses, PtyGetErrors, PtyGetResponses, PtyListErrors, PtyListResponses, PtyRemoveErrors, PtyRemoveResponses, PtyShellsErrors, PtyShellsResponses, PtyUpdateErrors, PtyUpdateResponses, QuestionAnswer, QuestionListErrors, QuestionListResponses, QuestionRejectErrors, QuestionRejectResponses, QuestionReplyErrors, QuestionReplyResponses, QuestionV2Reply, SessionAbortErrors, SessionAbortResponses, SessionChildrenErrors, SessionChildrenResponses, SessionCommandErrors, SessionCommandResponses, SessionCreateErrors, SessionCreateResponses, SessionDeleteErrors, SessionDeleteMessageErrors, SessionDeleteMessageResponses, SessionDeleteResponses, SessionDiffErrors, SessionDiffResponses, SessionForkErrors, SessionForkResponses, SessionGetErrors, SessionGetResponses, SessionInitErrors, SessionInitResponses, SessionListErrors, SessionListResponses, SessionMessageErrors, SessionMessageResponses, SessionMessagesErrors, SessionMessagesResponses, SessionPromptAsyncErrors, SessionPromptAsyncResponses, SessionPromptErrors, SessionPromptResponses, SessionRevertErrors, SessionRevertResponses, SessionShareErrors, SessionShareResponses, SessionShellErrors, SessionShellResponses, SessionStatusErrors, SessionStatusResponses, SessionSummarizeErrors, SessionSummarizeResponses, SessionTodoErrors, SessionTodoResponses, SessionUnrevertErrors, SessionUnrevertResponses, SessionUnshareErrors, SessionUnshareResponses, SessionUpdateErrors, SessionUpdateResponses, SubtaskPartInput, SyncHistoryListErrors, SyncHistoryListResponses, SyncReplayErrors, SyncReplayResponses, SyncStartErrors, SyncStartResponses, SyncStealErrors, SyncStealResponses, TextPartInput, ToolIdsErrors, ToolIdsResponses, ToolListErrors, ToolListResponses, TuiAppendPromptErrors, TuiAppendPromptResponses, TuiClearPromptErrors, TuiClearPromptResponses, TuiControlNextErrors, TuiControlNextResponses, TuiControlResponseErrors, TuiControlResponseResponses, TuiExecuteCommandErrors, TuiExecuteCommandResponses, TuiOpenHelpErrors, TuiOpenHelpResponses, TuiOpenModelsErrors, TuiOpenModelsResponses, TuiOpenSessionsErrors, TuiOpenSessionsResponses, TuiOpenThemesErrors, TuiOpenThemesResponses, TuiPublishErrors, TuiPublishResponses, TuiSelectSessionErrors, TuiSelectSessionResponses, TuiShowToastErrors, TuiShowToastResponses, TuiSubmitPromptErrors, TuiSubmitPromptResponses, V2AgentListErrors, V2AgentListResponses, V2CommandListErrors, V2CommandListResponses, V2CredentialRemoveErrors, V2CredentialRemoveResponses, V2CredentialUpdateErrors, V2CredentialUpdateResponses, V2EventSubscribeResponses, V2FsFindErrors, V2FsFindResponses, V2FsListErrors, V2FsListResponses, V2FsReadErrors, V2FsReadResponses, V2HealthGetErrors, V2HealthGetResponses, V2IntegrationAttemptCancelErrors, V2IntegrationAttemptCancelResponses, V2IntegrationAttemptCompleteErrors, V2IntegrationAttemptCompleteResponses, V2IntegrationAttemptStatusErrors, V2IntegrationAttemptStatusResponses, V2IntegrationConnectKeyErrors, V2IntegrationConnectKeyResponses, V2IntegrationConnectOauthErrors, V2IntegrationConnectOauthResponses, V2IntegrationGetErrors, V2IntegrationGetResponses, V2IntegrationListErrors, V2IntegrationListResponses, V2LocationGetErrors, V2LocationGetResponses, V2ModelListErrors, V2ModelListResponses, V2PermissionRequestListErrors, V2PermissionRequestListResponses, V2PermissionSavedListErrors, V2PermissionSavedListResponses, V2PermissionSavedRemoveErrors, V2PermissionSavedRemoveResponses, V2ProjectCopyCreateErrors, V2ProjectCopyCreateResponses, V2ProjectCopyRefreshErrors, V2ProjectCopyRefreshResponses, V2ProjectCopyRemoveErrors, V2ProjectCopyRemoveResponses, V2ProviderGetErrors, V2ProviderGetResponses, V2ProviderListErrors, V2ProviderListResponses, V2PtyConnectErrors, V2PtyConnectResponses, V2PtyConnectTokenErrors, V2PtyConnectTokenResponses, V2PtyCreateErrors, V2PtyCreateResponses, V2PtyGetErrors, V2PtyGetResponses, V2PtyListErrors, V2PtyListResponses, V2PtyRemoveErrors, V2PtyRemoveResponses, V2PtyUpdateErrors, V2PtyUpdateResponses, V2QuestionRequestListErrors, V2QuestionRequestListResponses, V2ReferenceListErrors, V2ReferenceListResponses, V2SessionActiveErrors, V2SessionActiveResponses, V2SessionCompactErrors, V2SessionCompactResponses, V2SessionContextErrors, V2SessionContextResponses, V2SessionCreateErrors, V2SessionCreateResponses, V2SessionEventsResponses, V2SessionGetErrors, V2SessionGetResponses, V2SessionHistoryErrors, V2SessionHistoryResponses, V2SessionInterruptErrors, V2SessionInterruptResponses, V2SessionListErrors, V2SessionListResponses, V2SessionMessageErrors, V2SessionMessageResponses, V2SessionMessagesErrors, V2SessionMessagesResponses, V2SessionPermissionCreateErrors, V2SessionPermissionCreateResponses, V2SessionPermissionGetErrors, V2SessionPermissionGetResponses, V2SessionPermissionListErrors, V2SessionPermissionListResponses, V2SessionPermissionReplyErrors, V2SessionPermissionReplyResponses, V2SessionPromptErrors, V2SessionPromptResponses, V2SessionQuestionListErrors, V2SessionQuestionListResponses, V2SessionQuestionRejectErrors, V2SessionQuestionRejectResponses, V2SessionQuestionReplyErrors, V2SessionQuestionReplyResponses, V2SessionRevertClearErrors, V2SessionRevertClearResponses, V2SessionRevertCommitErrors, V2SessionRevertCommitResponses, V2SessionRevertStageErrors, V2SessionRevertStageResponses, V2SessionSwitchAgentErrors, V2SessionSwitchAgentResponses, V2SessionSwitchModelErrors, V2SessionSwitchModelResponses, V2SessionWaitErrors, V2SessionWaitResponses, V2SkillListErrors, V2SkillListResponses, VcsApplyErrors, VcsApplyResponses, VcsDiffErrors, VcsDiffRawErrors, VcsDiffRawResponses, VcsDiffResponses, VcsGetErrors, VcsGetResponses, VcsStatusErrors, VcsStatusResponses, WorktreeCreateErrors, WorktreeCreateInput, WorktreeCreateResponses, WorktreeListErrors, WorktreeListResponses, WorktreeRemoveErrors, WorktreeRemoveInput, WorktreeRemoveResponses, WorktreeResetErrors, WorktreeResetInput, WorktreeResetResponses } from "./types.gen.js";
export type Options<TData extends TDataShape = TDataShape, ThrowOnError extends boolean = boolean> = Options2<TData, ThrowOnError> & {
    /**
     * You can provide a client instance returned by `createClient()` instead of
     * individual options. This might be also useful if you want to implement a
     * custom client.
     */
    client?: Client;
    /**
     * You can pass arbitrary values through the `meta` object. This can be
     * used to access values that aren't defined as part of the SDK function.
     */
    meta?: Record<string, unknown>;
};
declare class HeyApiClient {
    protected client: Client;
    constructor(args?: {
        client?: Client;
    });
}
declare class HeyApiRegistry<T> {
    private readonly defaultKey;
    private readonly instances;
    get(key?: string): T;
    set(value: T, key?: string): void;
}
export declare class Auth extends HeyApiClient {
    /**
     * Remove auth credentials
     *
     * Remove authentication credentials
     */
    remove<ThrowOnError extends boolean = false>(parameters: {
        providerID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<AuthRemoveResponses, AuthRemoveErrors, ThrowOnError, "fields">;
    /**
     * Set auth credentials
     *
     * Set authentication credentials
     */
    set<ThrowOnError extends boolean = false>(parameters: {
        providerID: string;
        auth?: Auth3;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<AuthSetResponses, AuthSetErrors, ThrowOnError, "fields">;
}
export declare class App extends HeyApiClient {
    /**
     * Write log
     *
     * Write a log entry to the server logs with specified level and metadata.
     */
    log<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        service?: string;
        level?: "debug" | "info" | "error" | "warn";
        message?: string;
        extra?: {
            [key: string]: unknown;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<AppLogResponses, AppLogErrors, ThrowOnError, "fields">;
    /**
     * List agents
     *
     * Get a list of all available AI agents in the OpenCode system.
     */
    agents<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<AppAgentsResponses, AppAgentsErrors, ThrowOnError, "fields">;
    /**
     * List skills
     *
     * Get a list of all available skills in the OpenCode system.
     */
    skills<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<AppSkillsResponses, AppSkillsErrors, ThrowOnError, "fields">;
}
export declare class ControlPlane extends HeyApiClient {
    /**
     * Move session
     *
     * Move a session to another project directory, optionally transferring local changes.
     */
    moveSession<ThrowOnError extends boolean = false>(parameters?: {
        sessionID?: string;
        destination?: MoveSessionDestination;
        moveChanges?: boolean;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalControlPlaneMoveSessionResponses, ExperimentalControlPlaneMoveSessionErrors, ThrowOnError, "fields">;
}
export declare class Capabilities extends HeyApiClient {
    /**
     * Get experimental capabilities
     *
     * Get experimental features enabled on the OpenCode server.
     */
    get<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalCapabilitiesGetResponses, ExperimentalCapabilitiesGetErrors, ThrowOnError, "fields">;
}
export declare class Console extends HeyApiClient {
    /**
     * Get active Console provider metadata
     *
     * Get the active Console org name and the set of provider IDs managed by that Console org.
     */
    get<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalConsoleGetResponses, ExperimentalConsoleGetErrors, ThrowOnError, "fields">;
    /**
     * List switchable Console orgs
     *
     * Get the available Console orgs across logged-in accounts, including the current active org.
     */
    listOrgs<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalConsoleListOrgsResponses, ExperimentalConsoleListOrgsErrors, ThrowOnError, "fields">;
    /**
     * Switch active Console org
     *
     * Persist a new active Console account/org selection for the current local OpenCode state.
     */
    switchOrg<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        accountID?: string;
        orgID?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalConsoleSwitchOrgResponses, unknown, ThrowOnError, "fields">;
}
export declare class Session extends HeyApiClient {
    /**
     * List sessions
     *
     * Get a list of all OpenCode sessions across projects, sorted by most recently updated. Archived sessions are excluded by default.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        roots?: boolean | "true" | "false";
        start?: number;
        cursor?: number;
        search?: string;
        limit?: number;
        archived?: boolean | "true" | "false";
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalSessionListResponses, ExperimentalSessionListErrors, ThrowOnError, "fields">;
    /**
     * Background subagents
     *
     * Detach any synchronous subagents currently blocking the session and continue them in the background.
     */
    background<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalSessionBackgroundResponses, ExperimentalSessionBackgroundErrors, ThrowOnError, "fields">;
}
export declare class Resource extends HeyApiClient {
    /**
     * Get MCP resources
     *
     * Get all available MCP resources from connected servers. Optionally filter by name.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalResourceListResponses, ExperimentalResourceListErrors, ThrowOnError, "fields">;
}
export declare class ProjectCopy extends HeyApiClient {
    /**
     * Generate project copy name
     *
     * Generate a short name for a project copy from task context.
     */
    generateName<ThrowOnError extends boolean = false>(parameters: {
        projectID: string;
        directory?: string;
        workspace?: string;
        context?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalProjectCopyGenerateNameResponses, ExperimentalProjectCopyGenerateNameErrors, ThrowOnError, "fields">;
}
export declare class Adapter extends HeyApiClient {
    /**
     * List workspace adapters
     *
     * List all available workspace adapters for the current project.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalWorkspaceAdapterListResponses, ExperimentalWorkspaceAdapterListErrors, ThrowOnError, "fields">;
}
export declare class Workspace extends HeyApiClient {
    /**
     * List workspaces
     *
     * List all workspaces.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalWorkspaceListResponses, ExperimentalWorkspaceListErrors, ThrowOnError, "fields">;
    /**
     * Create workspace
     *
     * Create a workspace for the current project.
     */
    create<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        id?: string;
        type?: string;
        branch?: string | null;
        extra?: unknown | null;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalWorkspaceCreateResponses, ExperimentalWorkspaceCreateErrors, ThrowOnError, "fields">;
    /**
     * Sync workspace list
     *
     * Register missing workspaces returned by workspace adapters.
     */
    syncList<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalWorkspaceSyncListResponses, ExperimentalWorkspaceSyncListErrors, ThrowOnError, "fields">;
    /**
     * Workspace status
     *
     * Get connection status for workspaces in the current project.
     */
    status<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalWorkspaceStatusResponses, ExperimentalWorkspaceStatusErrors, ThrowOnError, "fields">;
    /**
     * Remove workspace
     *
     * Remove an existing workspace.
     */
    remove<ThrowOnError extends boolean = false>(parameters: {
        id: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalWorkspaceRemoveResponses, ExperimentalWorkspaceRemoveErrors, ThrowOnError, "fields">;
    /**
     * Warp session into workspace
     *
     * Move a session's sync history into the target workspace, or detach it to the local project.
     */
    warp<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        id?: string | null;
        sessionID?: string;
        copyChanges?: boolean;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ExperimentalWorkspaceWarpResponses, ExperimentalWorkspaceWarpErrors, ThrowOnError, "fields">;
    private _adapter?;
    get adapter(): Adapter;
}
export declare class Experimental extends HeyApiClient {
    private _controlPlane?;
    get controlPlane(): ControlPlane;
    private _capabilities?;
    get capabilities(): Capabilities;
    private _console?;
    get console(): Console;
    private _session?;
    get session(): Session;
    private _resource?;
    get resource(): Resource;
    private _projectCopy?;
    get projectCopy(): ProjectCopy;
    private _workspace?;
    get workspace(): Workspace;
}
export declare class Config extends HeyApiClient {
    /**
     * Get global configuration
     *
     * Retrieve the current global OpenCode configuration settings and preferences.
     */
    get<ThrowOnError extends boolean = false>(options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<GlobalConfigGetResponses, GlobalConfigGetErrors, ThrowOnError, "fields">;
    /**
     * Update global configuration
     *
     * Update global OpenCode configuration settings and preferences.
     */
    update<ThrowOnError extends boolean = false>(parameters?: {
        config?: Config3;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<GlobalConfigUpdateResponses, GlobalConfigUpdateErrors, ThrowOnError, "fields">;
}
export declare class Global extends HeyApiClient {
    /**
     * Get health
     *
     * Get health information about the OpenCode server.
     */
    health<ThrowOnError extends boolean = false>(options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<GlobalHealthResponses, GlobalHealthErrors, ThrowOnError, "fields">;
    /**
     * Get global events
     *
     * Subscribe to global events from the OpenCode system using server-sent events.
     */
    event<ThrowOnError extends boolean = false>(options?: Options<never, ThrowOnError>): Promise<import("./core/serverSentEvents.gen.js").ServerSentEventsResult<GlobalEventResponses>>;
    /**
     * Dispose instance
     *
     * Clean up and dispose all OpenCode instances, releasing all resources.
     */
    dispose<ThrowOnError extends boolean = false>(options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<GlobalDisposeResponses, GlobalDisposeErrors, ThrowOnError, "fields">;
    /**
     * Upgrade opencode
     *
     * Upgrade opencode to the specified version or latest if not specified.
     */
    upgrade<ThrowOnError extends boolean = false>(parameters?: {
        target?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<GlobalUpgradeResponses, GlobalUpgradeErrors, ThrowOnError, "fields">;
    private _config?;
    get config(): Config;
}
export declare class Event extends HeyApiClient {
    /**
     * Subscribe to events
     *
     * Get events
     */
    subscribe<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): Promise<import("./core/serverSentEvents.gen.js").ServerSentEventsResult<EventSubscribeResponses>>;
}
export declare class Config2 extends HeyApiClient {
    /**
     * Get configuration
     *
     * Retrieve the current OpenCode configuration settings and preferences.
     */
    get<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ConfigGetResponses, ConfigGetErrors, ThrowOnError, "fields">;
    /**
     * Update configuration
     *
     * Update OpenCode configuration settings and preferences.
     */
    update<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        config?: Config3;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ConfigUpdateResponses, ConfigUpdateErrors, ThrowOnError, "fields">;
    /**
     * List config providers
     *
     * Get a list of all configured AI providers and their default models.
     */
    providers<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ConfigProvidersResponses, ConfigProvidersErrors, ThrowOnError, "fields">;
}
export declare class Tool extends HeyApiClient {
    /**
     * List tools
     *
     * Get a list of available tools with their JSON schema parameters for a specific provider and model combination.
     */
    list<ThrowOnError extends boolean = false>(parameters: {
        directory?: string;
        workspace?: string;
        provider: string;
        model: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ToolListResponses, ToolListErrors, ThrowOnError, "fields">;
    /**
     * List tool IDs
     *
     * Get a list of all available tool IDs, including both built-in tools and dynamically registered tools.
     */
    ids<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ToolIdsResponses, ToolIdsErrors, ThrowOnError, "fields">;
}
export declare class Worktree extends HeyApiClient {
    /**
     * Remove worktree
     *
     * Remove a git worktree and delete its branch.
     */
    remove<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        worktreeRemoveInput?: WorktreeRemoveInput;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<WorktreeRemoveResponses, WorktreeRemoveErrors, ThrowOnError, "fields">;
    /**
     * List worktrees
     *
     * List all sandbox worktrees for the current project.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<WorktreeListResponses, WorktreeListErrors, ThrowOnError, "fields">;
    /**
     * Create worktree
     *
     * Create a new git worktree for the current project and run any configured startup scripts.
     */
    create<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        worktreeCreateInput?: WorktreeCreateInput;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<WorktreeCreateResponses, WorktreeCreateErrors, ThrowOnError, "fields">;
    /**
     * Reset worktree
     *
     * Reset a worktree branch to the primary default branch.
     */
    reset<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        worktreeResetInput?: WorktreeResetInput;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<WorktreeResetResponses, WorktreeResetErrors, ThrowOnError, "fields">;
}
export declare class Find extends HeyApiClient {
    /**
     * Find text
     *
     * Search for text patterns across files in the project using ripgrep.
     */
    text<ThrowOnError extends boolean = false>(parameters: {
        directory?: string;
        workspace?: string;
        pattern: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<FindTextResponses, FindTextErrors, ThrowOnError, "fields">;
    /**
     * Find files
     *
     * Search for files or directories by name or pattern in the project directory.
     */
    files<ThrowOnError extends boolean = false>(parameters: {
        directory?: string;
        workspace?: string;
        query: string;
        dirs?: "true" | "false";
        type?: "file" | "directory";
        limit?: number;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<FindFilesResponses, FindFilesErrors, ThrowOnError, "fields">;
    /**
     * Find symbols
     *
     * Search for workspace symbols like functions, classes, and variables using LSP.
     */
    symbols<ThrowOnError extends boolean = false>(parameters: {
        directory?: string;
        workspace?: string;
        query: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<FindSymbolsResponses, FindSymbolsErrors, ThrowOnError, "fields">;
}
export declare class File extends HeyApiClient {
    /**
     * List files
     *
     * List files and directories in a specified path.
     */
    list<ThrowOnError extends boolean = false>(parameters: {
        directory?: string;
        workspace?: string;
        path: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<FileListResponses, FileListErrors, ThrowOnError, "fields">;
    /**
     * Read file
     *
     * Read the content of a specified file.
     */
    read<ThrowOnError extends boolean = false>(parameters: {
        directory?: string;
        workspace?: string;
        path: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<FileReadResponses, FileReadErrors, ThrowOnError, "fields">;
    /**
     * Get file status
     *
     * Get the git status of all files in the project.
     */
    status<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<FileStatusResponses, FileStatusErrors, ThrowOnError, "fields">;
}
export declare class Instance extends HeyApiClient {
    /**
     * Dispose instance
     *
     * Clean up and dispose the current OpenCode instance, releasing all resources.
     */
    dispose<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<InstanceDisposeResponses, InstanceDisposeErrors, ThrowOnError, "fields">;
}
export declare class Path extends HeyApiClient {
    /**
     * Get paths
     *
     * Retrieve the current working directory and related path information for the OpenCode instance.
     */
    get<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PathGetResponses, PathGetErrors, ThrowOnError, "fields">;
}
export declare class Diff extends HeyApiClient {
    /**
     * Get raw VCS diff
     *
     * Retrieve a raw patch for current uncommitted changes.
     */
    raw<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<VcsDiffRawResponses, VcsDiffRawErrors, ThrowOnError, "fields">;
}
export declare class Vcs extends HeyApiClient {
    /**
     * Get VCS info
     *
     * Retrieve version control system (VCS) information for the current project, such as git branch.
     */
    get<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<VcsGetResponses, VcsGetErrors, ThrowOnError, "fields">;
    /**
     * Get VCS status
     *
     * Retrieve changed files in the current working tree without patches.
     */
    status<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<VcsStatusResponses, VcsStatusErrors, ThrowOnError, "fields">;
    /**
     * Get VCS diff
     *
     * Retrieve the current git diff for the working tree or against the default branch.
     */
    diff<ThrowOnError extends boolean = false>(parameters: {
        directory?: string;
        workspace?: string;
        mode: "git" | "branch";
        context?: number;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<VcsDiffResponses, VcsDiffErrors, ThrowOnError, "fields">;
    /**
     * Apply VCS patch
     *
     * Apply a raw patch to the current working tree.
     */
    apply<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        patch?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<VcsApplyResponses, VcsApplyErrors, ThrowOnError, "fields">;
    private _diff?;
    get diff2(): Diff;
}
export declare class Command extends HeyApiClient {
    /**
     * List commands
     *
     * Get a list of all available commands in the OpenCode system.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<CommandListResponses, CommandListErrors, ThrowOnError, "fields">;
}
export declare class Lsp extends HeyApiClient {
    /**
     * Get LSP status
     *
     * Get LSP server status
     */
    status<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<LspStatusResponses, LspStatusErrors, ThrowOnError, "fields">;
}
export declare class Formatter extends HeyApiClient {
    /**
     * Get formatter status
     *
     * Get formatter status
     */
    status<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<FormatterStatusResponses, FormatterStatusErrors, ThrowOnError, "fields">;
}
export declare class Auth2 extends HeyApiClient {
    /**
     * Remove MCP OAuth
     *
     * Remove OAuth credentials for an MCP server.
     */
    remove<ThrowOnError extends boolean = false>(parameters: {
        name: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<McpAuthRemoveResponses, McpAuthRemoveErrors, ThrowOnError, "fields">;
    /**
     * Start MCP OAuth
     *
     * Start OAuth authentication flow for a Model Context Protocol (MCP) server.
     */
    start<ThrowOnError extends boolean = false>(parameters: {
        name: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<McpAuthStartResponses, McpAuthStartErrors, ThrowOnError, "fields">;
    /**
     * Complete MCP OAuth
     *
     * Complete OAuth authentication for a Model Context Protocol (MCP) server using the authorization code.
     */
    callback<ThrowOnError extends boolean = false>(parameters: {
        name: string;
        directory?: string;
        workspace?: string;
        code?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<McpAuthCallbackResponses, McpAuthCallbackErrors, ThrowOnError, "fields">;
    /**
     * Authenticate MCP OAuth
     *
     * Start OAuth flow and wait for callback (opens browser).
     */
    authenticate<ThrowOnError extends boolean = false>(parameters: {
        name: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<McpAuthAuthenticateResponses, McpAuthAuthenticateErrors, ThrowOnError, "fields">;
}
export declare class Mcp extends HeyApiClient {
    /**
     * Get MCP status
     *
     * Get the status of all Model Context Protocol (MCP) servers.
     */
    status<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<McpStatusResponses, McpStatusErrors, ThrowOnError, "fields">;
    /**
     * Add MCP server
     *
     * Dynamically add a new Model Context Protocol (MCP) server to the system.
     */
    add<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        name?: string;
        config?: McpLocalConfig | McpRemoteConfig;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<McpAddResponses, McpAddErrors, ThrowOnError, "fields">;
    /**
     * Connect an MCP server.
     */
    connect<ThrowOnError extends boolean = false>(parameters: {
        name: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<McpConnectResponses, McpConnectErrors, ThrowOnError, "fields">;
    /**
     * Disconnect an MCP server.
     */
    disconnect<ThrowOnError extends boolean = false>(parameters: {
        name: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<McpDisconnectResponses, McpDisconnectErrors, ThrowOnError, "fields">;
    private _auth?;
    get auth(): Auth2;
}
export declare class Project extends HeyApiClient {
    /**
     * List all projects
     *
     * Get a list of projects that have been opened with OpenCode.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ProjectListResponses, ProjectListErrors, ThrowOnError, "fields">;
    /**
     * Get current project
     *
     * Retrieve the currently active project that OpenCode is working with.
     */
    current<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ProjectCurrentResponses, ProjectCurrentErrors, ThrowOnError, "fields">;
    /**
     * Initialize git repository
     *
     * Create a git repository for the current project and return the refreshed project info.
     */
    initGit<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ProjectInitGitResponses, ProjectInitGitErrors, ThrowOnError, "fields">;
    /**
     * Update project
     *
     * Update project properties such as name, icon, and commands.
     */
    update<ThrowOnError extends boolean = false>(parameters: {
        projectID: string;
        directory?: string;
        workspace?: string;
        name?: string;
        icon?: ProjectIcon;
        commands?: ProjectCommands;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ProjectUpdateResponses, ProjectUpdateErrors, ThrowOnError, "fields">;
    /**
     * List project directories
     *
     * List known local absolute directories for a project.
     */
    directories<ThrowOnError extends boolean = false>(parameters: {
        projectID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ProjectDirectoriesResponses, ProjectDirectoriesErrors, ThrowOnError, "fields">;
}
export declare class Pty extends HeyApiClient {
    /**
     * List available shells
     *
     * Get a list of available shells on the system.
     */
    shells<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PtyShellsResponses, PtyShellsErrors, ThrowOnError, "fields">;
    /**
     * List PTY sessions
     *
     * Get a list of all active pseudo-terminal (PTY) sessions managed by OpenCode.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PtyListResponses, PtyListErrors, ThrowOnError, "fields">;
    /**
     * Create PTY session
     *
     * Create a new pseudo-terminal (PTY) session for running shell commands and processes.
     */
    create<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        command?: string;
        args?: Array<string>;
        cwd?: string;
        title?: string;
        env?: {
            [key: string]: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PtyCreateResponses, PtyCreateErrors, ThrowOnError, "fields">;
    /**
     * Remove PTY session
     *
     * Remove and terminate a specific pseudo-terminal (PTY) session.
     */
    remove<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PtyRemoveResponses, PtyRemoveErrors, ThrowOnError, "fields">;
    /**
     * Get PTY session
     *
     * Retrieve detailed information about a specific pseudo-terminal (PTY) session.
     */
    get<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PtyGetResponses, PtyGetErrors, ThrowOnError, "fields">;
    /**
     * Update PTY session
     *
     * Update properties of an existing pseudo-terminal (PTY) session.
     */
    update<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        directory?: string;
        workspace?: string;
        title?: string;
        size?: {
            rows: number;
            cols: number;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PtyUpdateResponses, PtyUpdateErrors, ThrowOnError, "fields">;
    /**
     * Create PTY WebSocket token
     *
     * Create a short-lived ticket for opening a PTY WebSocket connection.
     */
    connectToken<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PtyConnectTokenResponses, PtyConnectTokenErrors, ThrowOnError, "fields">;
    /**
     * Connect to PTY session
     *
     * Establish a WebSocket connection to interact with a pseudo-terminal (PTY) session in real-time.
     */
    connect<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        directory?: string;
        workspace?: string;
        cursor?: string;
        ticket?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PtyConnectResponses, PtyConnectErrors, ThrowOnError, "fields">;
}
export declare class Question extends HeyApiClient {
    /**
     * List pending questions
     *
     * Get all pending question requests across all sessions.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<QuestionListResponses, QuestionListErrors, ThrowOnError, "fields">;
    /**
     * Reply to question request
     *
     * Provide answers to a question request from the AI assistant.
     */
    reply<ThrowOnError extends boolean = false>(parameters: {
        requestID: string;
        directory?: string;
        workspace?: string;
        answers?: Array<QuestionAnswer>;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<QuestionReplyResponses, QuestionReplyErrors, ThrowOnError, "fields">;
    /**
     * Reject question request
     *
     * Reject a question request from the AI assistant.
     */
    reject<ThrowOnError extends boolean = false>(parameters: {
        requestID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<QuestionRejectResponses, QuestionRejectErrors, ThrowOnError, "fields">;
}
export declare class Permission extends HeyApiClient {
    /**
     * List pending permissions
     *
     * Get all pending permission requests across all sessions.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PermissionListResponses, PermissionListErrors, ThrowOnError, "fields">;
    /**
     * Respond to permission request
     *
     * Approve or deny a permission request from the AI assistant.
     */
    reply<ThrowOnError extends boolean = false>(parameters: {
        requestID: string;
        directory?: string;
        workspace?: string;
        reply?: "once" | "always" | "reject";
        message?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PermissionReplyResponses, PermissionReplyErrors, ThrowOnError, "fields">;
    /**
     * Respond to permission
     *
     * Approve or deny a permission request from the AI assistant.
     *
     * @deprecated
     */
    respond<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        permissionID: string;
        directory?: string;
        workspace?: string;
        response?: "once" | "always" | "reject";
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PermissionRespondResponses, PermissionRespondErrors, ThrowOnError, "fields">;
}
export declare class Oauth extends HeyApiClient {
    /**
     * Start OAuth authorization
     *
     * Start the OAuth authorization flow for a provider.
     */
    authorize<ThrowOnError extends boolean = false>(parameters: {
        providerID: string;
        directory?: string;
        workspace?: string;
        method?: number;
        inputs?: {
            [key: string]: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ProviderOauthAuthorizeResponses, ProviderOauthAuthorizeErrors, ThrowOnError, "fields">;
    /**
     * Handle OAuth callback
     *
     * Handle the OAuth callback from a provider after user authorization.
     */
    callback<ThrowOnError extends boolean = false>(parameters: {
        providerID: string;
        directory?: string;
        workspace?: string;
        method?: number;
        code?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ProviderOauthCallbackResponses, ProviderOauthCallbackErrors, ThrowOnError, "fields">;
}
export declare class Provider extends HeyApiClient {
    /**
     * List providers
     *
     * Get a list of all available AI providers, including both available and connected ones.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ProviderListResponses, ProviderListErrors, ThrowOnError, "fields">;
    /**
     * Get provider auth methods
     *
     * Retrieve available authentication methods for all AI providers.
     */
    auth<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<ProviderAuthResponses, ProviderAuthErrors, ThrowOnError, "fields">;
    private _oauth?;
    get oauth(): Oauth;
}
export declare class Session2 extends HeyApiClient {
    /**
     * List sessions
     *
     * Get a list of all OpenCode sessions, sorted by most recently updated.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        scope?: "project";
        path?: string;
        roots?: boolean | "true" | "false";
        start?: number;
        search?: string;
        limit?: number;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionListResponses, SessionListErrors, ThrowOnError, "fields">;
    /**
     * Create session
     *
     * Create a new OpenCode session for interacting with AI assistants and managing conversations.
     */
    create<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        parentID?: string;
        title?: string;
        agent?: string;
        model?: {
            id: string;
            providerID: string;
            variant?: string;
        };
        metadata?: {
            [key: string]: unknown;
        };
        permission?: PermissionRuleset;
        workspaceID?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionCreateResponses, SessionCreateErrors, ThrowOnError, "fields">;
    /**
     * Get session status
     *
     * Retrieve the current status of all sessions, including active, idle, and completed states.
     */
    status<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionStatusResponses, SessionStatusErrors, ThrowOnError, "fields">;
    /**
     * Delete session
     *
     * Delete a session and permanently remove all associated data, including messages and history.
     */
    delete<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionDeleteResponses, SessionDeleteErrors, ThrowOnError, "fields">;
    /**
     * Get session
     *
     * Retrieve detailed information about a specific OpenCode session.
     */
    get<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionGetResponses, SessionGetErrors, ThrowOnError, "fields">;
    /**
     * Update session
     *
     * Update properties of an existing session, such as title or other metadata.
     */
    update<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        title?: string;
        metadata?: {
            [key: string]: unknown;
        };
        permission?: PermissionRuleset;
        time?: {
            archived?: number;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionUpdateResponses, SessionUpdateErrors, ThrowOnError, "fields">;
    /**
     * Get session children
     *
     * Retrieve all child sessions that were forked from the specified parent session.
     */
    children<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionChildrenResponses, SessionChildrenErrors, ThrowOnError, "fields">;
    /**
     * Get session todos
     *
     * Retrieve the todo list associated with a specific session, showing tasks and action items.
     */
    todo<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionTodoResponses, SessionTodoErrors, ThrowOnError, "fields">;
    /**
     * Get message diff
     *
     * Get the file changes (diff) that resulted from a specific user message in the session.
     */
    diff<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        messageID?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionDiffResponses, SessionDiffErrors, ThrowOnError, "fields">;
    /**
     * Get session messages
     *
     * Retrieve all messages in a session, including user prompts and AI responses.
     */
    messages<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        limit?: number;
        before?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionMessagesResponses, SessionMessagesErrors, ThrowOnError, "fields">;
    /**
     * Send message
     *
     * Create and send a new message to a session, streaming the AI response.
     */
    prompt<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        messageID?: string;
        model?: {
            providerID: string;
            modelID: string;
        };
        agent?: string;
        noReply?: boolean;
        tools?: {
            [key: string]: boolean;
        };
        format?: OutputFormat;
        system?: string;
        variant?: string;
        parts?: Array<TextPartInput | FilePartInput | AgentPartInput | SubtaskPartInput>;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionPromptResponses, SessionPromptErrors, ThrowOnError, "fields">;
    /**
     * Delete message
     *
     * Permanently delete a specific message and all of its parts from a session without reverting file changes.
     */
    deleteMessage<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        messageID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionDeleteMessageResponses, SessionDeleteMessageErrors, ThrowOnError, "fields">;
    /**
     * Get message
     *
     * Retrieve a specific message from a session by its message ID.
     */
    message<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        messageID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionMessageResponses, SessionMessageErrors, ThrowOnError, "fields">;
    /**
     * Fork session
     *
     * Create a new session by forking an existing session at a specific message point.
     */
    fork<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        messageID?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionForkResponses, SessionForkErrors, ThrowOnError, "fields">;
    /**
     * Abort session
     *
     * Abort an active session and stop any ongoing AI processing or command execution.
     */
    abort<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionAbortResponses, SessionAbortErrors, ThrowOnError, "fields">;
    /**
     * Initialize session
     *
     * Analyze the current application and create an AGENTS.md file with project-specific agent configurations.
     */
    init<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        modelID?: string;
        providerID?: string;
        messageID?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionInitResponses, SessionInitErrors, ThrowOnError, "fields">;
    /**
     * Unshare session
     *
     * Remove the shareable link for a session, making it private again.
     */
    unshare<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionUnshareResponses, SessionUnshareErrors, ThrowOnError, "fields">;
    /**
     * Share session
     *
     * Create a shareable link for a session, allowing others to view the conversation.
     */
    share<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionShareResponses, SessionShareErrors, ThrowOnError, "fields">;
    /**
     * Summarize session
     *
     * Generate a concise summary of the session using AI compaction to preserve key information.
     */
    summarize<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        providerID?: string;
        modelID?: string;
        auto?: boolean;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionSummarizeResponses, SessionSummarizeErrors, ThrowOnError, "fields">;
    /**
     * Send async message
     *
     * Create and send a new message to a session asynchronously, starting the session if needed and returning immediately.
     */
    promptAsync<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        messageID?: string;
        model?: {
            providerID: string;
            modelID: string;
        };
        agent?: string;
        noReply?: boolean;
        tools?: {
            [key: string]: boolean;
        };
        format?: OutputFormat;
        system?: string;
        variant?: string;
        parts?: Array<TextPartInput | FilePartInput | AgentPartInput | SubtaskPartInput>;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionPromptAsyncResponses, SessionPromptAsyncErrors, ThrowOnError, "fields">;
    /**
     * Send command
     *
     * Send a new command to a session for execution by the AI assistant.
     */
    command<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        messageID?: string;
        agent?: string;
        model?: string;
        arguments?: string;
        command?: string;
        variant?: string;
        parts?: Array<{
            id?: string;
            type: "file";
            mime: string;
            filename?: string;
            url: string;
            source?: FilePartSource;
        }>;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionCommandResponses, SessionCommandErrors, ThrowOnError, "fields">;
    /**
     * Run shell command
     *
     * Execute a shell command within the session context and return the AI's response.
     */
    shell<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        messageID?: string;
        agent?: string;
        model?: {
            providerID: string;
            modelID: string;
        };
        command?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionShellResponses, SessionShellErrors, ThrowOnError, "fields">;
    /**
     * Revert message
     *
     * Revert a specific message in a session, undoing its effects and restoring the previous state.
     */
    revert<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
        messageID?: string;
        partID?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionRevertResponses, SessionRevertErrors, ThrowOnError, "fields">;
    /**
     * Restore reverted messages
     *
     * Restore all previously reverted messages in a session.
     */
    unrevert<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SessionUnrevertResponses, SessionUnrevertErrors, ThrowOnError, "fields">;
}
export declare class Part extends HeyApiClient {
    /**
     * Delete a part from a message.
     */
    delete<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        messageID: string;
        partID: string;
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PartDeleteResponses, PartDeleteErrors, ThrowOnError, "fields">;
    /**
     * Update a part in a message.
     */
    update<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        messageID: string;
        partID: string;
        directory?: string;
        workspace?: string;
        part?: Part2;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<PartUpdateResponses, PartUpdateErrors, ThrowOnError, "fields">;
}
export declare class History extends HeyApiClient {
    /**
     * List sync events
     *
     * List sync events for all aggregates. Keys are aggregate IDs the client already knows about, values are the last known sequence ID. Events with seq > value are returned for those aggregates. Aggregates not listed in the input get their full history.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        body?: {
            [key: string]: number;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SyncHistoryListResponses, SyncHistoryListErrors, ThrowOnError, "fields">;
}
export declare class Sync extends HeyApiClient {
    /**
     * Start workspace sync
     *
     * Start sync loops for workspaces in the current project that have active sessions.
     */
    start<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SyncStartResponses, SyncStartErrors, ThrowOnError, "fields">;
    /**
     * Replay sync events
     *
     * Validate and replay a complete sync event history.
     */
    replay<ThrowOnError extends boolean = false>(parameters?: {
        query_directory?: string;
        workspace?: string;
        body_directory?: string;
        events?: Array<{
            id: string;
            aggregateID: string;
            seq: number;
            type: string;
            data: {
                [key: string]: unknown;
            };
        }>;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SyncReplayResponses, SyncReplayErrors, ThrowOnError, "fields">;
    /**
     * Steal session into workspace
     *
     * Update a session to belong to the current workspace through the sync event system.
     */
    steal<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        sessionID?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<SyncStealResponses, SyncStealErrors, ThrowOnError, "fields">;
    private _history?;
    get history(): History;
}
export declare class Control extends HeyApiClient {
    /**
     * Get next TUI request
     *
     * Retrieve the next TUI request from the queue for processing.
     */
    next<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiControlNextResponses, TuiControlNextErrors, ThrowOnError, "fields">;
    /**
     * Submit TUI response
     *
     * Submit a response to the TUI request queue to complete a pending request.
     */
    response<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        body?: unknown;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiControlResponseResponses, TuiControlResponseErrors, ThrowOnError, "fields">;
}
export declare class Tui extends HeyApiClient {
    /**
     * Append TUI prompt
     *
     * Append prompt to the TUI.
     */
    appendPrompt<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        text?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiAppendPromptResponses, TuiAppendPromptErrors, ThrowOnError, "fields">;
    /**
     * Open help dialog
     *
     * Open the help dialog in the TUI to display user assistance information.
     */
    openHelp<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiOpenHelpResponses, TuiOpenHelpErrors, ThrowOnError, "fields">;
    /**
     * Open sessions dialog
     *
     * Open the session dialog.
     */
    openSessions<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiOpenSessionsResponses, TuiOpenSessionsErrors, ThrowOnError, "fields">;
    /**
     * Open themes dialog
     *
     * Open the theme dialog.
     */
    openThemes<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiOpenThemesResponses, TuiOpenThemesErrors, ThrowOnError, "fields">;
    /**
     * Open models dialog
     *
     * Open the model dialog.
     */
    openModels<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiOpenModelsResponses, TuiOpenModelsErrors, ThrowOnError, "fields">;
    /**
     * Submit TUI prompt
     *
     * Submit the prompt.
     */
    submitPrompt<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiSubmitPromptResponses, TuiSubmitPromptErrors, ThrowOnError, "fields">;
    /**
     * Clear TUI prompt
     *
     * Clear the prompt.
     */
    clearPrompt<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiClearPromptResponses, TuiClearPromptErrors, ThrowOnError, "fields">;
    /**
     * Execute TUI command
     *
     * Execute a TUI command.
     */
    executeCommand<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        command?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiExecuteCommandResponses, TuiExecuteCommandErrors, ThrowOnError, "fields">;
    /**
     * Show TUI toast
     *
     * Show a toast notification in the TUI.
     */
    showToast<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        title?: string;
        message?: string;
        variant?: "info" | "success" | "warning" | "error";
        duration?: number;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiShowToastResponses, TuiShowToastErrors, ThrowOnError, "fields">;
    /**
     * Publish TUI event
     *
     * Publish a TUI event.
     */
    publish<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        body?: EventTuiPromptAppend | EventTuiCommandExecute | EventTuiToastShow | EventTuiSessionSelect;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiPublishResponses, TuiPublishErrors, ThrowOnError, "fields">;
    /**
     * Select session
     *
     * Navigate the TUI to display the specified session.
     */
    selectSession<ThrowOnError extends boolean = false>(parameters?: {
        directory?: string;
        workspace?: string;
        sessionID?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<TuiSelectSessionResponses, TuiSelectSessionErrors, ThrowOnError, "fields">;
    private _control?;
    get control(): Control;
}
export declare class Health extends HeyApiClient {
    /**
     * Check server health
     *
     * Check whether the API server is ready to accept requests.
     */
    get<ThrowOnError extends boolean = false>(options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2HealthGetResponses, V2HealthGetErrors, ThrowOnError, "fields">;
}
export declare class Location extends HeyApiClient {
    /**
     * Get location
     *
     * Resolve the requested location or the server default location.
     */
    get<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2LocationGetResponses, V2LocationGetErrors, ThrowOnError, "fields">;
}
export declare class Agent extends HeyApiClient {
    /**
     * List agents
     *
     * Retrieve currently registered agents.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2AgentListResponses, V2AgentListErrors, ThrowOnError, "fields">;
}
export declare class Revert extends HeyApiClient {
    /**
     * Stage session revert
     *
     * Stage or move a reversible session boundary and optionally apply its file changes.
     */
    stage<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        messageID?: string;
        files?: boolean;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionRevertStageResponses, V2SessionRevertStageErrors, ThrowOnError, "fields">;
    /**
     * Clear staged revert
     */
    clear<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionRevertClearResponses, V2SessionRevertClearErrors, ThrowOnError, "fields">;
    /**
     * Commit staged revert
     */
    commit<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionRevertCommitResponses, V2SessionRevertCommitErrors, ThrowOnError, "fields">;
}
export declare class Permission2 extends HeyApiClient {
    /**
     * List session permission requests
     *
     * Retrieve pending permission requests owned by a session.
     */
    list<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionPermissionListResponses, V2SessionPermissionListErrors, ThrowOnError, "fields">;
    /**
     * Create permission request
     *
     * Evaluate and, when approval is required, create a permission request for a session.
     */
    create<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        id?: string;
        action?: string;
        resources?: Array<string>;
        save?: Array<string>;
        metadata?: {
            [key: string]: unknown;
        };
        source?: PermissionV2Source;
        agent?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionPermissionCreateResponses, V2SessionPermissionCreateErrors, ThrowOnError, "fields">;
    /**
     * Get permission request
     *
     * Retrieve a pending permission request owned by a session.
     */
    get<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        requestID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionPermissionGetResponses, V2SessionPermissionGetErrors, ThrowOnError, "fields">;
    /**
     * Reply to pending permission request
     *
     * Respond to a pending permission request owned by a session.
     */
    reply<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        requestID: string;
        reply?: PermissionV2Reply;
        message?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionPermissionReplyResponses, V2SessionPermissionReplyErrors, ThrowOnError, "fields">;
}
export declare class Question2 extends HeyApiClient {
    /**
     * List session question requests
     *
     * Retrieve pending question requests owned by a session.
     */
    list<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionQuestionListResponses, V2SessionQuestionListErrors, ThrowOnError, "fields">;
    /**
     * Reply to pending question request
     *
     * Answer a pending question request owned by a session.
     */
    reply<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        requestID: string;
        questionV2Reply: QuestionV2Reply;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionQuestionReplyResponses, V2SessionQuestionReplyErrors, ThrowOnError, "fields">;
    /**
     * Reject pending question request
     *
     * Reject a pending question request owned by a session.
     */
    reject<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        requestID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionQuestionRejectResponses, V2SessionQuestionRejectErrors, ThrowOnError, "fields">;
}
export declare class Session3 extends HeyApiClient {
    /**
     * List sessions
     *
     * Retrieve sessions in the requested order. Items keep that order across pages; use cursor.next or cursor.previous to move through the ordered list.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        workspace?: string;
        limit?: number;
        order?: "asc" | "desc";
        search?: string;
        directory?: string;
        project?: string;
        subpath?: string;
        cursor?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionListResponses, V2SessionListErrors, ThrowOnError, "fields">;
    /**
     * Create session
     *
     * Create a session at the requested location.
     */
    create<ThrowOnError extends boolean = false>(parameters?: {
        id?: string;
        agent?: string;
        model?: ModelRef;
        location?: LocationRef;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionCreateResponses, V2SessionCreateErrors, ThrowOnError, "fields">;
    /**
     * List active sessions
     *
     * Retrieve foreground Session drains currently owned by this OpenCode process. Sessions absent from the result are inactive.
     */
    active<ThrowOnError extends boolean = false>(options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionActiveResponses, V2SessionActiveErrors, ThrowOnError, "fields">;
    /**
     * Get session
     *
     * Retrieve a session by ID.
     */
    get<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionGetResponses, V2SessionGetErrors, ThrowOnError, "fields">;
    /**
     * Switch session agent
     *
     * Switch the agent used by subsequent provider turns.
     */
    switchAgent<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        agent?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionSwitchAgentResponses, V2SessionSwitchAgentErrors, ThrowOnError, "fields">;
    /**
     * Switch session model
     *
     * Switch the model used by subsequent provider turns.
     */
    switchModel<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        model?: ModelRef;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionSwitchModelResponses, V2SessionSwitchModelErrors, ThrowOnError, "fields">;
    /**
     * Send message
     *
     * Durably admit one session input and schedule agent-loop execution unless resume is false.
     */
    prompt<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        id?: string;
        prompt?: PromptInput;
        delivery?: "steer" | "queue";
        resume?: boolean;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionPromptResponses, V2SessionPromptErrors, ThrowOnError, "fields">;
    /**
     * Compact session
     *
     * Compact a session conversation.
     */
    compact<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionCompactResponses, V2SessionCompactErrors, ThrowOnError, "fields">;
    /**
     * Wait for session
     *
     * Wait for a session agent loop to become idle.
     */
    wait<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionWaitResponses, V2SessionWaitErrors, ThrowOnError, "fields">;
    /**
     * Get session context
     *
     * Retrieve the active context messages for a session (all messages after the last compaction).
     */
    context<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionContextResponses, V2SessionContextErrors, ThrowOnError, "fields">;
    /**
     * Get session history
     *
     * Read one finite page of public durable Session events after an exclusive aggregate sequence. Newly committed events may appear on later pages.
     */
    history<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        limit?: number;
        after?: number;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionHistoryResponses, V2SessionHistoryErrors, ThrowOnError, "fields">;
    /**
     * Subscribe to session events
     *
     * Replay durable events after an aggregate sequence, then continue with new durable events.
     */
    events<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        after?: string;
    }, options?: Options<never, ThrowOnError>): Promise<import("./core/serverSentEvents.gen.js").ServerSentEventsResult<V2SessionEventsResponses>>;
    /**
     * Interrupt session execution
     *
     * Interrupt active execution owned by this OpenCode process. Idle interruption is a no-op.
     */
    interrupt<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionInterruptResponses, V2SessionInterruptErrors, ThrowOnError, "fields">;
    /**
     * Get session message
     *
     * Retrieve one projected message owned by the Session.
     */
    message<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        messageID: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionMessageResponses, V2SessionMessageErrors, ThrowOnError, "fields">;
    /**
     * Get session messages
     *
     * Retrieve projected messages for a session. Items keep the requested order across pages; use cursor.next or cursor.previous to move through the ordered timeline.
     */
    messages<ThrowOnError extends boolean = false>(parameters: {
        sessionID: string;
        limit?: number;
        order?: "asc" | "desc";
        cursor?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SessionMessagesResponses, V2SessionMessagesErrors, ThrowOnError, "fields">;
    private _revert?;
    get revert(): Revert;
    private _permission?;
    get permission(): Permission2;
    private _question?;
    get question(): Question2;
}
export declare class Model extends HeyApiClient {
    /**
     * List models
     *
     * Retrieve available models ordered by release date.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2ModelListResponses, V2ModelListErrors, ThrowOnError, "fields">;
}
export declare class Provider2 extends HeyApiClient {
    /**
     * List providers
     *
     * Retrieve active AI providers so clients can show provider availability and configuration.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2ProviderListResponses, V2ProviderListErrors, ThrowOnError, "fields">;
    /**
     * Get provider
     *
     * Retrieve a single AI provider so clients can inspect its availability and endpoint settings.
     */
    get<ThrowOnError extends boolean = false>(parameters: {
        providerID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2ProviderGetResponses, V2ProviderGetErrors, ThrowOnError, "fields">;
}
export declare class Connect extends HeyApiClient {
    /**
     * Connect with key
     *
     * Run a key authentication method and store the resulting credential.
     */
    key<ThrowOnError extends boolean = false>(parameters: {
        integrationID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
        key?: string;
        label?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2IntegrationConnectKeyResponses, V2IntegrationConnectKeyErrors, ThrowOnError, "fields">;
    /**
     * Begin OAuth connection
     *
     * Start an OAuth attempt and return the authorization details.
     */
    oauth<ThrowOnError extends boolean = false>(parameters: {
        integrationID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
        methodID?: string;
        inputs?: {
            [key: string]: string;
        };
        label?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2IntegrationConnectOauthResponses, V2IntegrationConnectOauthErrors, ThrowOnError, "fields">;
}
export declare class Attempt extends HeyApiClient {
    /**
     * Cancel OAuth connection
     *
     * Cancel an OAuth attempt and release its resources.
     */
    cancel<ThrowOnError extends boolean = false>(parameters: {
        attemptID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2IntegrationAttemptCancelResponses, V2IntegrationAttemptCancelErrors, ThrowOnError, "fields">;
    /**
     * Get OAuth attempt status
     *
     * Poll the current status of an OAuth attempt.
     */
    status<ThrowOnError extends boolean = false>(parameters: {
        attemptID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2IntegrationAttemptStatusResponses, V2IntegrationAttemptStatusErrors, ThrowOnError, "fields">;
    /**
     * Complete OAuth connection
     *
     * Complete a code-based OAuth attempt and store the resulting credential.
     */
    complete<ThrowOnError extends boolean = false>(parameters: {
        attemptID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
        code?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2IntegrationAttemptCompleteResponses, V2IntegrationAttemptCompleteErrors, ThrowOnError, "fields">;
}
export declare class Integration extends HeyApiClient {
    /**
     * List integrations
     *
     * Retrieve available integrations and their authentication methods.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2IntegrationListResponses, V2IntegrationListErrors, ThrowOnError, "fields">;
    /**
     * Get integration
     *
     * Retrieve one integration and its authentication methods.
     */
    get<ThrowOnError extends boolean = false>(parameters: {
        integrationID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2IntegrationGetResponses, V2IntegrationGetErrors, ThrowOnError, "fields">;
    private _connect?;
    get connect(): Connect;
    private _attempt?;
    get attempt(): Attempt;
}
export declare class Credential extends HeyApiClient {
    /**
     * Remove credential
     *
     * Remove a stored integration credential.
     */
    remove<ThrowOnError extends boolean = false>(parameters: {
        credentialID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2CredentialRemoveResponses, V2CredentialRemoveErrors, ThrowOnError, "fields">;
    /**
     * Update credential
     *
     * Update a stored credential label.
     */
    update<ThrowOnError extends boolean = false>(parameters: {
        credentialID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
        label?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2CredentialUpdateResponses, V2CredentialUpdateErrors, ThrowOnError, "fields">;
}
export declare class Request extends HeyApiClient {
    /**
     * List pending permission requests
     *
     * Retrieve pending permission requests for a location.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PermissionRequestListResponses, V2PermissionRequestListErrors, ThrowOnError, "fields">;
}
export declare class Saved extends HeyApiClient {
    /**
     * List saved permissions
     *
     * Retrieve saved permissions, optionally filtered by project.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        projectID?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PermissionSavedListResponses, V2PermissionSavedListErrors, ThrowOnError, "fields">;
    /**
     * Remove saved permission
     *
     * Remove a saved permission by ID.
     */
    remove<ThrowOnError extends boolean = false>(parameters: {
        id: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PermissionSavedRemoveResponses, V2PermissionSavedRemoveErrors, ThrowOnError, "fields">;
}
export declare class Permission3 extends HeyApiClient {
    private _request?;
    get request(): Request;
    private _saved?;
    get saved(): Saved;
}
export declare class Fs extends HeyApiClient {
    /**
     * Read file
     *
     * Serve one file relative to the requested location.
     */
    read<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2FsReadResponses, V2FsReadErrors, ThrowOnError, "fields">;
    /**
     * List directory
     *
     * List direct children of one directory relative to the requested location.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
        path?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2FsListResponses, V2FsListErrors, ThrowOnError, "fields">;
    /**
     * Find files
     *
     * Find recursively ranked filesystem entries relative to the requested location.
     */
    find<ThrowOnError extends boolean = false>(parameters: {
        location?: {
            directory?: string;
            workspace?: string;
        };
        query: string;
        type?: "file" | "directory";
        limit?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2FsFindResponses, V2FsFindErrors, ThrowOnError, "fields">;
}
export declare class Command2 extends HeyApiClient {
    /**
     * List commands
     *
     * Retrieve currently registered commands.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2CommandListResponses, V2CommandListErrors, ThrowOnError, "fields">;
}
export declare class Skill extends HeyApiClient {
    /**
     * List skills
     *
     * Retrieve currently registered skills.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2SkillListResponses, V2SkillListErrors, ThrowOnError, "fields">;
}
export declare class Event2 extends HeyApiClient {
    /**
     * Subscribe to events
     *
     * Subscribe to native event payloads for the server.
     */
    subscribe<ThrowOnError extends boolean = false>(options?: Options<never, ThrowOnError>): Promise<import("./core/serverSentEvents.gen.js").ServerSentEventsResult<V2EventSubscribeResponses>>;
}
export declare class Pty2 extends HeyApiClient {
    /**
     * List PTY sessions
     *
     * List PTY sessions for a location, including exited sessions retained until removal.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PtyListResponses, V2PtyListErrors, ThrowOnError, "fields">;
    /**
     * Create PTY session
     *
     * Create a pseudo-terminal session for a location.
     */
    create<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
        command?: string;
        args?: Array<string>;
        cwd?: string;
        title?: string;
        env?: {
            [key: string]: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PtyCreateResponses, V2PtyCreateErrors, ThrowOnError, "fields">;
    /**
     * Remove PTY session
     *
     * Terminate and remove one PTY session.
     */
    remove<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PtyRemoveResponses, V2PtyRemoveErrors, ThrowOnError, "fields">;
    /**
     * Get PTY session
     *
     * Get one PTY session, including its exit code once exited.
     */
    get<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PtyGetResponses, V2PtyGetErrors, ThrowOnError, "fields">;
    /**
     * Update PTY session
     *
     * Update the title or viewport size of one PTY session.
     */
    update<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
        title?: string;
        size?: {
            rows: number;
            cols: number;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PtyUpdateResponses, V2PtyUpdateErrors, ThrowOnError, "fields">;
    /**
     * Create PTY WebSocket token
     *
     * Create a short-lived single-use ticket for opening a PTY WebSocket connection.
     */
    connectToken<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PtyConnectTokenResponses, V2PtyConnectTokenErrors, ThrowOnError, "fields">;
    /**
     * Connect to PTY session
     *
     * Establish a WebSocket connection streaming PTY output and accepting terminal input.
     */
    connect<ThrowOnError extends boolean = false>(parameters: {
        ptyID: string;
        "location[directory]"?: string;
        "location[workspace]"?: string;
        cursor?: string;
        ticket?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2PtyConnectResponses, V2PtyConnectErrors, ThrowOnError, "fields">;
}
export declare class Request2 extends HeyApiClient {
    /**
     * List pending question requests
     *
     * Retrieve pending question requests for a location.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2QuestionRequestListResponses, V2QuestionRequestListErrors, ThrowOnError, "fields">;
}
export declare class Question3 extends HeyApiClient {
    private _request?;
    get request(): Request2;
}
export declare class Reference extends HeyApiClient {
    /**
     * List references
     *
     * List references available in the requested location.
     */
    list<ThrowOnError extends boolean = false>(parameters?: {
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2ReferenceListResponses, V2ReferenceListErrors, ThrowOnError, "fields">;
}
export declare class ProjectCopy2 extends HeyApiClient {
    remove<ThrowOnError extends boolean = false>(parameters: {
        projectID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
        directory?: string;
        force?: boolean;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2ProjectCopyRemoveResponses, V2ProjectCopyRemoveErrors, ThrowOnError, "fields">;
    create<ThrowOnError extends boolean = false>(parameters: {
        projectID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
        strategy?: string;
        directory?: string;
        name?: string;
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2ProjectCopyCreateResponses, V2ProjectCopyCreateErrors, ThrowOnError, "fields">;
    refresh<ThrowOnError extends boolean = false>(parameters: {
        projectID: string;
        location?: {
            directory?: string;
            workspace?: string;
        };
    }, options?: Options<never, ThrowOnError>): import("./client/types.gen.js").RequestResult<V2ProjectCopyRefreshResponses, V2ProjectCopyRefreshErrors, ThrowOnError, "fields">;
}
export declare class V2 extends HeyApiClient {
    private _health?;
    get health(): Health;
    private _location?;
    get location(): Location;
    private _agent?;
    get agent(): Agent;
    private _session?;
    get session(): Session3;
    private _model?;
    get model(): Model;
    private _provider?;
    get provider(): Provider2;
    private _integration?;
    get integration(): Integration;
    private _credential?;
    get credential(): Credential;
    private _permission?;
    get permission(): Permission3;
    private _fs?;
    get fs(): Fs;
    private _command?;
    get command(): Command2;
    private _skill?;
    get skill(): Skill;
    private _event?;
    get event(): Event2;
    private _pty?;
    get pty(): Pty2;
    private _question?;
    get question(): Question3;
    private _reference?;
    get reference(): Reference;
    private _projectCopy?;
    get projectCopy(): ProjectCopy2;
}
export declare class OpencodeClient extends HeyApiClient {
    static readonly __registry: HeyApiRegistry<OpencodeClient>;
    constructor(args?: {
        client?: Client;
        key?: string;
    });
    private _auth?;
    get auth(): Auth;
    private _app?;
    get app(): App;
    private _experimental?;
    get experimental(): Experimental;
    private _global?;
    get global(): Global;
    private _event?;
    get event(): Event;
    private _config?;
    get config(): Config2;
    private _tool?;
    get tool(): Tool;
    private _worktree?;
    get worktree(): Worktree;
    private _find?;
    get find(): Find;
    private _file?;
    get file(): File;
    private _instance?;
    get instance(): Instance;
    private _path?;
    get path(): Path;
    private _vcs?;
    get vcs(): Vcs;
    private _command?;
    get command(): Command;
    private _lsp?;
    get lsp(): Lsp;
    private _formatter?;
    get formatter(): Formatter;
    private _mcp?;
    get mcp(): Mcp;
    private _project?;
    get project(): Project;
    private _pty?;
    get pty(): Pty;
    private _question?;
    get question(): Question;
    private _permission?;
    get permission(): Permission;
    private _provider?;
    get provider(): Provider;
    private _session?;
    get session(): Session2;
    private _part?;
    get part(): Part;
    private _sync?;
    get sync(): Sync;
    private _tui?;
    get tui(): Tui;
    private _v2?;
    get v2(): V2;
}
export {};
