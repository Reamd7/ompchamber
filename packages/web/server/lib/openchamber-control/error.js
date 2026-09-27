/**
 * OMPChamber 控制/会话动作的统一错误类型与归一化助手。
 *
 * OMPChamberControlError 携带 HTTP statusCode 与任意附加详情字段（如
 * partial/partialAction/goalConfigured），供 routes 层原样映射为 JSON
 * 错误响应；asControlError 把任意抛出值归一为该类型，保留可识别的
 * message 与 statusCode。
 */
/** 带 HTTP 状态码与附加详情的控制动作错误；routes 层按它写响应。 */
export class OMPChamberControlError extends Error {
  /** message 为错误文案；statusCode 为 HTTP 状态（默认 500）；details 的键值直接挂到错误对象上。 */
  constructor(message, statusCode = 500, details = {}) {
    super(message);
    // 固定错误名，便于 instanceof 与日志识别。
    this.name = 'OMPChamberControlError';
    // HTTP 状态码，routes 层直接采用。
    this.statusCode = statusCode;
    // 附加详情（partial、goalConfigured 等）平铺到错误对象上。
    Object.assign(this, details);
  }
}

/**
 * 把任意错误归一为 OMPChamberControlError：已是该类型则原样返回；
 * Error 取其 message，其余用 fallbackMessage。statusCode 取
 * error.statusCode（可数）否则 fallbackStatus；goalConfigured === true
 * 时透传该标记（goal 已配置的错误需要向调用方披露）。
 */
export const asControlError = (error, fallbackMessage, fallbackStatus = 500) => {
  if (error instanceof OMPChamberControlError) return error;
  const message = error instanceof Error ? error.message : fallbackMessage;
  return new OMPChamberControlError(message || fallbackMessage, Number(error?.statusCode) || fallbackStatus, {
    ...(error?.goalConfigured === true ? { goalConfigured: true } : {}),
  });
};
