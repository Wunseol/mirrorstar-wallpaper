// 统一日志模块：按环境降级
// dev：info/warn/error 全部输出到 console
// prod：info 静默（import.meta.env.DEV 编译期替换为 false，dead code 会被移除），warn/error 保留

const isDev = import.meta.env.DEV;

export const log = {
  info: (...args: unknown[]) => {
    if (isDev) console.log(...args);
  },
  warn: (...args: unknown[]) => {
    console.warn(...args);
  },
  error: (...args: unknown[]) => {
    console.error(...args);
  },
};
