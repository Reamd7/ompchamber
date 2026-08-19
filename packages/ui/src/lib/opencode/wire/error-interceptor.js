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
export function wrapClientError(error, response, request, opts) {
    if (!opts?.throwOnError)
        return error;
    if (error instanceof Error)
        return error;
    // NamedError-shaped responses (the common case for opencode 4xx) come
    // through as POJOs — extract a useful message first, then wrap.
    if (typeof error === "object" && error !== null && Object.keys(error).length > 0) {
        const obj = error;
        const message = (typeof obj.data?.message === "string" && obj.data.message) ||
            (typeof obj.message === "string" && obj.message) ||
            (typeof obj.name === "string" && obj.name) ||
            describe(request, response);
        return new Error(message, { cause: { body: error, status: response?.status } });
    }
    if (typeof error === "string" && error.length > 0) {
        return new Error(error, { cause: { body: error, status: response?.status } });
    }
    // Empty body / network failure / undefined / null / empty object.
    const reason = response ? "(empty response body)" : "network error (no response)";
    return new Error(`opencode server ${describe(request, response)}: ${reason}`, {
        cause: { body: error, status: response?.status },
    });
}
function describe(request, response) {
    const method = request?.method ?? "?";
    const url = request?.url ?? "?";
    const status = response?.status;
    const statusText = response?.statusText;
    return `${method} ${url}${status ? " → " + status : ""}${statusText ? " " + statusText : ""}`;
}
