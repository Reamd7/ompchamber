/*
 * Vendored from @opencode-ai/sdk@1.18.18 (MIT License, Copyright OpenCode contributors).
 * This is OMPChamber's owned wire contract: the omp host (packages/web/server/lib/omp-host,
 * engine @oh-my-pi/pi-coding-agent) implements this API surface. Generated code is
 * unmodified except for this header and the error-interceptor import path.
 */
export * from "./gen/types.gen.js";
export type { FileSystemEntry as LocationFileSystemEntry } from "./gen/types.gen.js";
import { type Config } from "./gen/client/types.gen.js";
import { OpencodeClient } from "./gen/sdk.gen.js";
export { type Config as OpencodeClientConfig, OpencodeClient };
export declare function createOpencodeClient(config?: Config & {
    directory?: string;
    experimental_workspaceID?: string;
}): OpencodeClient;
