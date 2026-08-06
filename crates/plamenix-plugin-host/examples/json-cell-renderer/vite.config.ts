import { resolve } from 'node:path';
import { defineConfig } from 'vite';

// Builds the plugin's UI bundle to `dist/ui.mjs`. Mirrors the
// externalisation recipe documented in `plamenix/docs/plugin-authoring.md`
// — react / react-dom / react/jsx-runtime / lucide-react come from the
// host at runtime via the import map the shell injects at boot.
export default defineConfig({
  build: {
    lib: {
      entry: resolve(__dirname, 'src/index.tsx'),
      formats: ['es'],
      fileName: () => 'ui.mjs',
    },
    rollupOptions: {
      external: ['react', 'react-dom', 'react/jsx-runtime', 'lucide-react'],
    },
    target: 'es2022',
    minify: false,
    sourcemap: true,
  },
});
