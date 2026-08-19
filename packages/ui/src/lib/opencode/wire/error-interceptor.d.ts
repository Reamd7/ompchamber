/*
 * Vendored from @opencode-ai/sdk@1.18.18 (MIT License, Copyright OpenCode contributors).
 * This is OpenChamber's owned wire contract: the omp host (packages/web/server/lib/omp-host,
 * engine @oh-my-pi/pi-coding-agent) implements this API surface. Generated code is
 * unmodified except for this header and the error-interceptor import path.
 */
/**
 * Wrap whatever the generated client decoded from a non-2xx error body
 * into a real `Error` so downstream formatters (TUI, plugins) get a
 * useful `.message` instead of `[object Object]` or blank. The original
 * parsed body and status live under `.cause` for callers that need
 * structured fields.
 *
 * Only fires when the caller used `{ throwOnError: true }`. Callers that
 * read `result.error` directly (the result-tuple path) get the parsed
 * body unchanged so existing field-level reads (`.error.name`,
 * `JSON.stringify(error)`, etc.) are byte-for-byte identical to before.
 */
export declare function wrapClientError(error: unknown, response: Response | undefined, request: Request | undefined, opts: {
    throwOnError?: boolean;
} | undefined): unknown;
