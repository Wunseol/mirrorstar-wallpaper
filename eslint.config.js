import js from "@eslint/js";
import tseslint from "typescript-eslint";
import eslintConfigPrettier from "eslint-config-prettier";
import globals from "globals";

// ESLint 9 flat config：typescript-eslint recommended + prettier 兼容
// 规则适度放宽以避免大量改动现有代码
export default tseslint.config(
  {
    ignores: ["src-tauri/**", "node_modules/**", "dist/**", "lively-reference/**"],
  },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  eslintConfigPrettier,
  {
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "module",
      globals: {
        ...globals.browser,
        ...globals.node,
      },
    },
    rules: {
      // 日志统一走 logger 模块，不再直接使用 console（logger.ts 内部除外）
      // 从 "off" 改为 "warn"，与 no-explicit-any 一致的渐进策略
      "no-console": "warn",
      // 现有代码存在少量 any（如 debounce 工具函数），先 warn 提示，逐步消除后改为 error
      "@typescript-eslint/no-explicit-any": "warn",
      // 未使用变量阻断构建（当前 0 warnings），忽略下划线前缀参数
      "@typescript-eslint/no-unused-vars": ["error", { argsIgnorePattern: "^_", varsIgnorePattern: "^_" }],
      // 允许 require（vite.config.ts 等可能用到）
      "@typescript-eslint/no-require-imports": "off",
    },
  },
  // overrides 对 logger.ts 放行 no-console（logger 内部使用 console 是其设计职责）
  {
    files: ["src/scripts/utils/logger.ts"],
    rules: {
      "no-console": "off",
    },
  },
  // state.ts 使用 console.warn 是有意为之（避免与 logger.ts 循环依赖），
  // 配合 lint 脚本 --max-warnings 0 强制零警告，需对此文件放行 no-console
  {
    files: ["src/scripts/state.ts"],
    rules: {
      "no-console": "off",
    },
  },
);
