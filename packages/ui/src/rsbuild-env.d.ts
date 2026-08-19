/// <reference types="@rsbuild/core/types" />

interface Window {
    __opencodeDebug?: {
        getLastAssistantMessage: () => unknown;
        getAllMessages: (truncate?: boolean) => unknown[];
        truncateMessages: (messages: unknown[]) => unknown[];
        getAppStatus: () => Promise<unknown>;
        checkLastMessage: () => boolean;
        findEmptyMessages: (message: unknown[]) => unknown[];
        showRetryHelp: () => void;
        getStreamingState: () => unknown;
        analyzeMessageCompletionConsistency: (options?: unknown) => unknown;
        checkCompletionStatus: () => unknown;
    };
}
