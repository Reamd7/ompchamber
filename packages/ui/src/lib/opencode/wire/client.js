/*
 * Vendored from @opencode-ai/sdk@1.18.18 (MIT License, Copyright OpenCode contributors).
 * This is OMPChamber's owned wire contract: the omp host (packages/web/server/lib/omp-host,
 * engine @oh-my-pi/pi-coding-agent) implements this API surface. Generated code is
 * unmodified except for this header and the error-interceptor import path.
 */
export * from "./gen/types.gen.js";
import { createClient } from "./gen/client/client.gen.js";
import { OpencodeClient } from "./gen/sdk.gen.js";
import { wrapClientError } from "./error-interceptor.js";
export { OpencodeClient };
function pick(value, fallback, encode) {
    if (!value)
        return;
    if (!fallback)
        return value;
    if (value === fallback)
        return fallback;
    if (encode && value === encode(fallback))
        return fallback;
    return value;
}
function rewrite(request, values) {
    if (request.method !== "GET" && request.method !== "HEAD")
        return request;
    const url = new URL(request.url);
    let changed = false;
    for (const [name, key] of [
        ["x-opencode-directory", "directory"],
        ["x-opencode-workspace", "workspace"],
    ]) {
        const value = pick(request.headers.get(name), key === "directory" ? values.directory : values.workspace, key === "directory" ? encodeURIComponent : undefined);
        if (!value)
            continue;
        for (const query of url.pathname.startsWith("/api/") ? [key, `location[${key}]`] : [key]) {
            if (!url.searchParams.has(query)) {
                url.searchParams.set(query, value);
            }
        }
        changed = true;
    }
    if (!changed)
        return request;
    const next = new Request(url, request);
    next.headers.delete("x-opencode-directory");
    next.headers.delete("x-opencode-workspace");
    return next;
}
export function createOpencodeClient(config) {
    if (!config?.fetch) {
        const customFetch = (req) => {
            // @ts-ignore
            req.timeout = false;
            return fetch(req);
        };
        config = {
            ...config,
            fetch: customFetch,
        };
    }
    if (config?.directory) {
        config.headers = {
            ...config.headers,
            "x-opencode-directory": encodeURIComponent(config.directory),
        };
    }
    if (config?.experimental_workspaceID) {
        config.headers = {
            ...config.headers,
            "x-opencode-workspace": config.experimental_workspaceID,
        };
    }
    const client = createClient(config);
    client.interceptors.request.use((request) => rewrite(request, {
        directory: config?.directory,
        workspace: config?.experimental_workspaceID,
    }));
    client.interceptors.response.use((response) => {
        const contentType = response.headers.get("content-type");
        if (contentType === "text/html")
            throw new Error("Request is not supported by this version of OpenCode Server (Server responded with text/html)");
        return response;
    });
    client.interceptors.error.use(wrapClientError);
    return new OpencodeClient({ client });
}
