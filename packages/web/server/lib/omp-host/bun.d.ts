// Ambient types for the Bun runtime surface the omp host and its bun:test
// suite use. The workspace does not depend on bun-types, so this file declares
// exactly the touched API; nothing more. If bun-types is ever added as a
// dependency, delete this file and use the official types instead.
/**
 * （中文模块说明）Bun 运行时的环境类型声明：只声明 omp 宿主与
 * bun:test 套件实际触碰的 API 面，不多不少。工作区不依赖 bun-types；
 * 若将来引入官方类型包，删除本文件改用官方类型即可。
 */

/** Bun 全局对象（本文件只声明 serve 一项）。 */
declare const Bun: {
  serve(options: {
    /** 监听主机名（支持 IPv6 字面量）。 */
    hostname?: string;
    /** 监听端口；0 表示由系统分配。 */
    port?: number;
    /** Seconds; Bun caps this at 255. */
    /** 空闲超时（秒）；Bun 上限 255。 */
    idleTimeout?: number;
    /** 每个请求的入口：返回 Response 或其 Promise。 */
    fetch: (request: Request) => Response | Promise<Response>;
  }): {
    /** 实际监听的主机名。 */
    hostname: string;
    /** 实际监听的端口。 */
    port: number;
    /** 停止服务器；入参表示是否同时断开活动连接。 */
    stop(closeActiveConnections?: boolean): void;
  };
};

// import.meta.main is NOT redeclared here: @types/node (v24.2+) already
// declares it with the same boolean semantics Bun uses.

/** NodeJS 全局命名空间的最小合并声明（仅补 ProcessVersions）。 */
declare namespace NodeJS {
/** 进程版本表（process.versions）的类型补全。 */
  interface ProcessVersions {
    /** 进程运行于 Bun 运行时时存在。 */
    /** Present when the process runs under the Bun runtime. */
    bun?: string;
  }
}

// Bun's import.meta.dir: absolute dirname of the current module. @types/node
// declares import.meta.main but not dir; __filename/__dirname are CJS globals.
/** Bun 的 import.meta.dir：当前模块的绝对目录名。 */
interface ImportMeta {
  /** 当前模块的绝对 dirname（Bun 专有；@types/node 未声明）。 */
  dir: string;
}

declare module 'bun:test' {
  /** 注册一个具名测试用例（同步或异步函数体）。 */
  export function test(name: string, fn: () => void | Promise<void>): void;
  /** test 的命名空间挂载（test.each 等）。 */
  export namespace test {
    /** bun's test.each: table rows are spread as the fn's arguments. */
    export const each: (
      cases: readonly unknown[],
    ) => (name: string, fn: (...args: unknown[]) => void | Promise<void>) => void;
  }
  /** 注册一个具名测试套件（describe 块）。 */
  export function describe(name: string, fn: () => void): void;
  /** 全部用例结束后执行一次的钩子（同步或异步）。 */
  export function afterAll(fn: () => void | Promise<void>): void;

  /** 递归的部分形状（subset），供 toMatchObject/objectContaining 使用。 */
/** Recursive subset shape used by toMatchObject/objectContaining. */
type DeepPartial<T> = T extends (infer U)[]
  ? readonly DeepPartial<U>[]
  : T extends object
    ? { [K in keyof T]?: DeepPartial<T[K]> }
    : T;
  /** 本套件用到的匹配器子集（按实际值类型 T 泛型化）：对称匹配器要求
   *  期望值可赋给 T，容器匹配器抽取元素类型，toMatchObject 的子集
   *  匹配不对称性用 DeepPartial 表达。 */
  /**
   * Matchers limited to the set this suite uses. Generic over the actual
   * value's type T: symmetric matchers (toBe/toEqual) require the expected
   * value to be assignable to T, container matchers extract the element
   * type, and the asymmetry of toMatchObject (subset match) is expressed
   * through Partial<T>.
   */
  export interface ExpectMatchers<T> {
    /** 严格相等（===）。 */
    toBe(expected: T): void;
    /** 深相等（递归比较）。 */
    toEqual(expected: T): void;
    /** 字符串含子串 / 数组含元素。 */
    toContain(expected: T extends string ? string : T extends readonly (infer E)[] ? E : T): void;
    /** 匹配正则或子串。 */
    toMatch(pattern: RegExp | string): void;
    /** 严格等于 null。 */
    toBeNull(): void;
    /** 严格等于 undefined。 */
    toBeUndefined(): void;
    /** 已定义（非 undefined）。 */
    toBeDefined(): void;
    /** 真值断言。 */
    toBeTruthy(): void;
    /** 假值断言。 */
    toBeFalsy(): void;
    /** 数值大于期望。 */
    toBeGreaterThan(expected: number): void;
    /** 数值大于等于期望。 */
    toBeGreaterThanOrEqual(expected: number): void;
    /** 数值小于期望。 */
    toBeLessThan(expected: number): void;
    /** length 属性等于期望。 */
    toHaveLength(expected: number): void;
    /** 接受子串/正则、Error 实例或构造器（TypeError 等）。 */
    /** Accepts a substring/regex, an Error instance, or a constructor (TypeError et al.). */
    toThrow(expected?: RegExp | string | Error | ErrorConstructor | Function): void;
    /** 以指定实参被调用过（参数类型从 mock.calls 推导）。 */
    toHaveBeenCalledWith(...args: (T extends { mock: { calls: (infer C)[] } } ? C : T extends (...args: infer A) => void ? A : never)): void;
    /** 最近一次调用使用了指定实参。 */
    toHaveBeenLastCalledWith(...args: (T extends { mock: { calls: (infer C)[] } } ? C : T extends (...args: infer A) => void ? A : never)): void;
    /** 恰好被调用 count 次。 */
    toHaveBeenCalledTimes(count: number): void;
    /** 至少被调用一次。 */
    toHaveBeenCalled(): void;
    /** 递归子集匹配（部分形状相等）。 */
    toMatchObject(expected: DeepPartial<T>): void;
    /** 拥有指定属性（可选断言属性值）。 */
    toHaveProperty(prop: string | (string | number | symbol)[], value?: T extends object ? T[keyof T] : T): void;
    /** 是给定构造器的实例。 */
    toBeInstanceOf(constructor: abstract new (...args: never[]) => object): void;
    /** 取反修饰：对后续匹配器断言相反结果。 */
    not: ExpectMatchers<T>;
    /** 拒绝态匹配：TS 的 Promise 类型不携带拒绝原因类型，这里保持
   *  宽松形状，只匹配实际抛出的值。 */
    /** Rejection reasons have no channel in TS promise types; these stay
     * shape-loose against the actually-thrown value. */
    rejects: {
      toMatchObject(expected: DeepPartial<object>): void;
      toThrow(expected?: RegExp | string): void;
    };
    /** 兑现态匹配：以 Awaited<T> 继续断言。 */
    resolves: ExpectMatchers<Awaited<T>>;
  }
  /** 断言入口：包裹实际值，返回链式匹配器（可带失败消息）。 */
  export function expect<T>(actual: T, message?: string): ExpectMatchers<T>;
  /** expect 的命名空间挂载（any/objectContaining 等占位匹配器）。 */
  export namespace expect {
    /** 占位匹配器：匹配给定构造器的任意实例（类型为 never 以便
   *  嵌入任何期望值位置）。 */
    /** Matcher placeholder: matches any instance of the given constructor.
     * Typed `never` so it slots into any expected-value position. */
    export function any(constructor: abstract new (...args: never[]) => object): never;
    /** 占位匹配器：在 toEqual 内做部分结构匹配。 */
    /** Matcher placeholder: partial structural match inside toEqual. */
    export function objectContaining<T extends object>(expected: DeepPartial<T>): T;
    /** 占位匹配器：在 toEqual 内做子串匹配。 */
    /** Matcher placeholder: substring match inside toEqual. */
    export function stringContaining(substring: string): string;
  }

  /** 把 fn 包成记录调用的 spy：声明的参数类型流入 mock.calls；
   *  无参箭头函数时 calls 放宽为 unknown[][]（运行时仍记录实际实参）。 */
  /**
   * Wraps fn in a spy that records invocations. Declared fn parameters flow
   * into mock.calls; when the arrow declares none, calls widens to
   * unknown[][] — the runtime still records the actual invocation args.
   */
  export function mock<A extends unknown[] = unknown[], R = void>(
    fn?: (...args: A) => R,
  ): ((...args: A) => R) & { mock: { calls: A[] } };
  /** mock 的命名空间挂载（mock.module 等）。 */
  export namespace mock {
    /** 在本测试文件后续范围内替换目标模块的导出。 */
    /** Replace a module's exports for the rest of the test file. */
    export function module<M extends object>(specifier: string, factory: () => M): void;
  }
}
