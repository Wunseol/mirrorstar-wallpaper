import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "jsdom",
    globals: true,
    include: ["src/**/*.test.ts"],
    coverage: {
      provider: "v8",
      reporter: ["text", "html"],
      include: ["src/**/*.ts"],
      // types.ts 为纯类型声明无运行时逻辑；mod.ts 为 barrel re-export，均无测试价值
      exclude: ["src/**/*.test.ts", "src/**/*.d.ts", "src/scripts/types.ts", "src/scripts/ui/mod.ts"],
      thresholds: {
        lines: 70,
        // 显式声明 branches 阈值与 statements/lines 对齐（由 60 提升至 70）
        branches: 70,
        functions: 70,
        // 补充 statements 阈值，与 lines 对齐，保持四项阈值完整
        statements: 70,
      },
    },
  },
});
