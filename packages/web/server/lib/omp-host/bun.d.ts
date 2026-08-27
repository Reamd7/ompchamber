// Ambient types for the Bun runtime surface the omp host and its bun:test
// suite use. The workspace does not depend on bun-types, so this file declares
// exactly the touched API; nothing more. If bun-types is ever added as a
// dependency, delete this file and use the official types instead.

declare const Bun: {
  serve(options: {
    hostname?: string;
    port?: number;
    /** Seconds; Bun caps this at 255. */
    idleTimeout?: number;
    fetch: (request: Request) => Response | Promise<Response>;
  }): {
    hostname: string;
    port: number;
    stop(closeActiveConnections?: boolean): void;
  };
};

// import.meta.main is NOT redeclared here: @types/node (v24.2+) already
// declares it with the same boolean semantics Bun uses.

declare namespace NodeJS {
  interface ProcessVersions {
    /** Present when the process runs under the Bun runtime. */
    bun?: string;
  }
}

// Bun's import.meta.dir: absolute dirname of the current module. @types/node
// declares import.meta.main but not dir; __filename/__dirname are CJS globals.
interface ImportMeta {
  dir: string;
}

declare module 'bun:test' {
  export function test(name: string, fn: () => void | Promise<void>): void;
  export namespace test {
    /** bun's test.each: table rows are spread as the fn's arguments. */
    export const each: (
      cases: readonly unknown[],
    ) => (name: string, fn: (...args: unknown[]) => void | Promise<void>) => void;
  }
  export function describe(name: string, fn: () => void): void;
  export function afterAll(fn: () => void | Promise<void>): void;

/** Recursive subset shape used by toMatchObject/objectContaining. */
type DeepPartial<T> = T extends (infer U)[]
  ? readonly DeepPartial<U>[]
  : T extends object
    ? { [K in keyof T]?: DeepPartial<T[K]> }
    : T;
  /**
   * Matchers limited to the set this suite uses. Generic over the actual
   * value's type T: symmetric matchers (toBe/toEqual) require the expected
   * value to be assignable to T, container matchers extract the element
   * type, and the asymmetry of toMatchObject (subset match) is expressed
   * through Partial<T>.
   */
  export interface ExpectMatchers<T> {
    toBe(expected: T): void;
    toEqual(expected: T): void;
    toContain(expected: T extends string ? string : T extends readonly (infer E)[] ? E : T): void;
    toMatch(pattern: RegExp | string): void;
    toBeNull(): void;
    toBeUndefined(): void;
    toBeDefined(): void;
    toBeTruthy(): void;
    toBeFalsy(): void;
    toBeGreaterThan(expected: number): void;
    toBeGreaterThanOrEqual(expected: number): void;
    toBeLessThan(expected: number): void;
    toHaveLength(expected: number): void;
    /** Accepts a substring/regex, an Error instance, or a constructor (TypeError et al.). */
    toThrow(expected?: RegExp | string | Error | ErrorConstructor | Function): void;
    toHaveBeenCalledWith(...args: (T extends { mock: { calls: (infer C)[] } } ? C : T extends (...args: infer A) => void ? A : never)): void;
    toHaveBeenLastCalledWith(...args: (T extends { mock: { calls: (infer C)[] } } ? C : T extends (...args: infer A) => void ? A : never)): void;
    toHaveBeenCalledTimes(count: number): void;
    toHaveBeenCalled(): void;
    toMatchObject(expected: DeepPartial<T>): void;
    toHaveProperty(prop: string | (string | number | symbol)[], value?: T extends object ? T[keyof T] : T): void;
    toBeInstanceOf(constructor: abstract new (...args: never[]) => object): void;
    not: ExpectMatchers<T>;
    /** Rejection reasons have no channel in TS promise types; these stay
     * shape-loose against the actually-thrown value. */
    rejects: {
      toMatchObject(expected: DeepPartial<object>): void;
      toThrow(expected?: RegExp | string): void;
    };
    resolves: ExpectMatchers<Awaited<T>>;
  }
  export function expect<T>(actual: T, message?: string): ExpectMatchers<T>;
  export namespace expect {
    /** Matcher placeholder: matches any instance of the given constructor.
     * Typed `never` so it slots into any expected-value position. */
    export function any(constructor: abstract new (...args: never[]) => object): never;
    /** Matcher placeholder: partial structural match inside toEqual. */
    export function objectContaining<T extends object>(expected: DeepPartial<T>): T;
    /** Matcher placeholder: substring match inside toEqual. */
    export function stringContaining(substring: string): string;
  }

  /**
   * Wraps fn in a spy that records invocations. Declared fn parameters flow
   * into mock.calls; when the arrow declares none, calls widens to
   * unknown[][] — the runtime still records the actual invocation args.
   */
  export function mock<A extends unknown[] = unknown[], R = void>(
    fn?: (...args: A) => R,
  ): ((...args: A) => R) & { mock: { calls: A[] } };
  export namespace mock {
    /** Replace a module's exports for the rest of the test file. */
    export function module<M extends object>(specifier: string, factory: () => M): void;
  }
}
