import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  server: {
    port: 8090,
    proxy: {
      '/api': { target: 'http://127.0.0.1:8081', changeOrigin: true },
      '/ws': { target: 'ws://127.0.0.1:8081', ws: true },
      '/gridws': { target: 'ws://127.0.0.1:8082', ws: true },
      '/api/grid-token': { target: 'http://127.0.0.1:8082', changeOrigin: true },
    },
  },
});
