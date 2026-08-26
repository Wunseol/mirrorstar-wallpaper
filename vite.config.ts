import { defineConfig } from "vite";

// 当前 build.minify 使用 terser（非默认 esbuild），目的为启用
// terserOptions.compress.pure_funcs = ["console.log"]，仅移除 console.log 而保留
// error/warn（logger 模块在生产环境仍可输出告警与错误）。
// esbuild 不支持 pure_funcs 语义，无法等价替代。terser 编译性能略低于 esbuild，
// 但前端体积小（<100KB），构建时间差异在可接受范围内。
//
// v16-D-015 取舍说明（已评估并接受）：
// - terser 比 esbuild 慢约 2-3x（esbuild native Go，terser JS 实现），
//   但增量构建差异在秒级，CI 无可观测影响
// - 若未来前端体积增长导致 build 时间显著上升，可评估：
//   ① 切换 esbuild + 在代码内 `if (import.meta.env.PROD) console.log = () => {}` 覆盖；
//   ② 保留 terser 但仅生产构建启用（`minify = NODE_ENV === 'production' ? 'terser' : 'esbuild'`）。

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 排除 Rust 构建产物目录（src-tauri/target 与 workspace 根 target/），
      // 否则 cargo 编译写 build script 时 Vite 文件监听触发 EBUSY 崩溃
      ignored: ["**/src-tauri/**", "**/target/**"],
    },
  },
  build: {
    minify: "terser",
    terserOptions: {
      compress: {
        // 仅移除 console.log（保留 error/warn，logger 模块仍可在生产环境输出告警与错误）
        pure_funcs: ["console.log"],
      },
    },
    // manualChunks 拆分 vendor 与应用代码，改善缓存命中率
    // 当前仅 @tauri-apps/api 为生产依赖，单独拆分便于 Tauri 版本升级时缓存失效最小化
    rollupOptions: {
      output: {
        manualChunks: {
          vendor: ["@tauri-apps/api"],
        },
      },
    },
  },
});
