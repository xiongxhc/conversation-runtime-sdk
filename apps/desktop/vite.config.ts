import { defineConfig, type PluginOption } from "vite";

type ReactPluginFactory = () => PluginOption;

const reactPluginModule = "@vitejs/plugin-react";
const reactPlugin = (await import(reactPluginModule)) as { default: ReactPluginFactory };

export default defineConfig(async () => ({
  plugins: [reactPlugin.default()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  test: {
    environment: "node",
    include: ["test/**/*.test.ts", "test/**/*.test.tsx"],
  },
}));
