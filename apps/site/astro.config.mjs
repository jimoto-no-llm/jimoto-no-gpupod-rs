// @ts-check
import { defineConfig } from "astro/config";
import react from "@astrojs/react";
import tailwindcss from "@tailwindcss/vite";

// 公開サイトは静的。ダッシュボード的な一覧は React islands が担う。
export default defineConfig({
  site: "https://gpupod.example.org",
  integrations: [react()],
  vite: { plugins: [tailwindcss()] },
  build: { format: "directory" },
  devToolbar: { enabled: false },
});
