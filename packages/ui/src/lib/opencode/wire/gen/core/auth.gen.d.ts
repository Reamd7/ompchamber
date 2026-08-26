/*
 * Vendored from @opencode-ai/sdk@1.18.18 (MIT License, Copyright OpenCode contributors).
 * This is OMPChamber's owned wire contract: the omp host (packages/web/server/lib/omp-host,
 * engine @oh-my-pi/pi-coding-agent) implements this API surface. Generated code is
 * unmodified except for this header and the error-interceptor import path.
 */
export type AuthToken = string | undefined;
export interface Auth {
    /**
     * Which part of the request do we use to send the auth?
     *
     * @default 'header'
     */
    in?: "header" | "query" | "cookie";
    /**
     * Header or query parameter name.
     *
     * @default 'Authorization'
     */
    name?: string;
    scheme?: "basic" | "bearer";
    type: "apiKey" | "http";
}
export declare const getAuthToken: (auth: Auth, callback: ((auth: Auth) => Promise<AuthToken> | AuthToken) | AuthToken) => Promise<string | undefined>;
