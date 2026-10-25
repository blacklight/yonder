import { defineConfig, mergeConfig } from "vitest/config";
import viteConfig from "./vite.config.ts";

export default defineConfig(async (env) =>
  mergeConfig(
    await (typeof viteConfig === "function" ? viteConfig(env) : viteConfig),
    {
      test: {
        environment: "jsdom",
        globals: true,
        setupFiles: ["src/test/setup.ts"],
        coverage: {
          provider: "v8",
          include: ["src/**/*.ts", "src/**/*.vue"],
          exclude: ["src/**/*.spec.ts", "src/test/**", "src/api/types.ts"],
        },
      },
    },
  ),
);
