/*
 * Vendored from @opencode-ai/sdk@1.18.18 (MIT License, Copyright OpenCode contributors).
 * This is OMPChamber's owned wire contract: the omp host (packages/web/server/lib/omp-host,
 * engine @oh-my-pi/pi-coding-agent) implements this API surface. Generated code is
 * unmodified except for this header and the error-interceptor import path.
 */
type Slot = "body" | "headers" | "path" | "query";
export type Field = {
    in: Exclude<Slot, "body">;
    /**
     * Field name. This is the name we want the user to see and use.
     */
    key: string;
    /**
     * Field mapped name. This is the name we want to use in the request.
     * If omitted, we use the same value as `key`.
     */
    map?: string;
} | {
    in: Extract<Slot, "body">;
    /**
     * Key isn't required for bodies.
     */
    key?: string;
    map?: string;
} | {
    /**
     * Field name. This is the name we want the user to see and use.
     */
    key: string;
    /**
     * Field mapped name. This is the name we want to use in the request.
     * If `in` is omitted, `map` aliases `key` to the transport layer.
     */
    map: Slot;
};
export interface Fields {
    allowExtra?: Partial<Record<Slot, boolean>>;
    args?: ReadonlyArray<Field>;
}
export type FieldsConfig = ReadonlyArray<Field | Fields>;
interface Params {
    body: unknown;
    headers: Record<string, unknown>;
    path: Record<string, unknown>;
    query: Record<string, unknown>;
}
export declare const buildClientParams: (args: ReadonlyArray<unknown>, fields: FieldsConfig) => Params;
export {};
