/*
 * Vendored from @opencode-ai/sdk@1.18.18 (MIT License, Copyright OpenCode contributors).
 * This is OMPChamber's owned wire contract: the omp host (packages/web/server/lib/omp-host,
 * engine @oh-my-pi/pi-coding-agent) implements this API surface. Generated code is
 * unmodified except for this header and the error-interceptor import path.
 */
interface SerializeOptions<T> extends SerializePrimitiveOptions, SerializerOptions<T> {
}
interface SerializePrimitiveOptions {
    allowReserved?: boolean;
    name: string;
}
export interface SerializerOptions<T> {
    /**
     * @default true
     */
    explode: boolean;
    style: T;
}
export type ArrayStyle = "form" | "spaceDelimited" | "pipeDelimited";
export type ArraySeparatorStyle = ArrayStyle | MatrixStyle;
type MatrixStyle = "label" | "matrix" | "simple";
export type ObjectStyle = "form" | "deepObject";
type ObjectSeparatorStyle = ObjectStyle | MatrixStyle;
interface SerializePrimitiveParam extends SerializePrimitiveOptions {
    value: string;
}
export declare const separatorArrayExplode: (style: ArraySeparatorStyle) => "." | ";" | "," | "&";
export declare const separatorArrayNoExplode: (style: ArraySeparatorStyle) => "," | "|" | "%20";
export declare const separatorObjectExplode: (style: ObjectSeparatorStyle) => "." | ";" | "," | "&";
export declare const serializeArrayParam: ({ allowReserved, explode, name, style, value, }: SerializeOptions<ArraySeparatorStyle> & {
    value: unknown[];
}) => string;
export declare const serializePrimitiveParam: ({ allowReserved, name, value }: SerializePrimitiveParam) => string;
export declare const serializeObjectParam: ({ allowReserved, explode, name, style, value, valueOnly, }: SerializeOptions<ObjectSeparatorStyle> & {
    value: Record<string, unknown> | Date;
    valueOnly?: boolean;
}) => string;
export {};
