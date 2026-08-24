import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  base: "/apps/",
  build: {
    emptyOutDir: true,
    outDir: "dist-ui",
  },
  plugins: [react()],
});
