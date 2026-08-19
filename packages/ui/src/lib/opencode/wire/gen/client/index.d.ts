/*
 * Vendored from @opencode-ai/sdk@1.18.18 (MIT License, Copyright OpenCode contributors).
 * This is OpenChamber's owned wire contract: the omp host (packages/web/server/lib/omp-host,
 * engine @oh-my-pi/pi-coding-agent) implements this API surface. Generated code is
 * unmodified except for this header and the error-interceptor import path.
 */
export type { Auth } from "../core/auth.gen.js";
export type { QuerySerializerOptions } from "../core/bodySerializer.gen.js";
export { formDataBodySerializer, jsonBodySerializer, urlSearchParamsBodySerializer, } from "../core/bodySerializer.gen.js";
export { buildClientParams } from "../core/params.gen.js";
export { serializeQueryKeyValue } from "../core/queryKeySerializer.gen.js";
export { createClient } from "./client.gen.js";
export type { Client, ClientOptions, Config, CreateClientConfig, Options, RequestOptions, RequestResult, ResolvedRequestOptions, ResponseStyle, TDataShape, } from "./types.gen.js";
export { createConfig, mergeHeaders } from "./utils.gen.js";
