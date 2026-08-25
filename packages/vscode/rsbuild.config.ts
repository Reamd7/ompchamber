import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineConfig, loadEnv } from '@rsbuild/core';
import { pluginReact } from '@rsbuild/plugin-react';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// The client keeps the historical VITE_ prefix so the documented
// VITE_OPENCODE_URL contract stays valid without renaming.
const { publicVars } = loadEnv({ prefixes: ['VITE_'] });

export default defineConfig(({ command }) => ({
  plugins: [
    pluginReact({
      reactCompiler: true,
      splitChunks: false,
    }),
  ],
  root: path.resolve(__dirname, '.'),
  source: {
    entry: {
      index: './webview/main.tsx',
    },
    define: {
      'process.env.NODE_ENV': JSON.stringify(command === 'build' ? 'production' : 'development'),
      global: 'globalThis',
      '__OMPCHAMBER_WEBVIEW_BUILD_TIME__': JSON.stringify(new Date().toISOString()),
    },
  },
  output: {
    // The extension loads the webview bundle itself (webviewHtml.ts) and
    // resolves asset URLs at runtime from the injected script's location, so
    // relative-against-script ('auto') prefixing keeps CSS and async chunks
    // resolvable from the webview resource root.
    assetPrefix: 'auto',
    module: true,
    filenameHash: false,
    distPath: {
      root: './dist/webview',
      js: 'assets',
      image: 'assets',
      font: 'assets',
      media: 'assets',
      svg: 'assets',
    },
    // vite emitted one initial script per page; keep license boilerplate out
    // of the packaged webview assets.
    legalComments: 'none',
  },
  performance: {
    // The extension loads a single known entry file (webviewHtml.ts builds
    // the document itself and references assets/index.js), so all initial
    // code must live in the entry chunk — async chunks still split.
    chunkSplit: { strategy: 'all-in-one' },
  },
  resolve: {
    alias: {
      '@ompchamber/ui': path.resolve(__dirname, '../ui/src'),
      '@vscode': path.resolve(__dirname, './webview'),
      '@': path.resolve(__dirname, '../ui/src'),
    },
  },
  html: {
    template: './webview/index.html',
  },
  server: {
    host: 'localhost',
    port: 5173,
    strictPort: true,
    cors: true,
    headers: {
      'Access-Control-Allow-Origin': '*',
    },
  },
  dev: {
    // The webview document lives on the vscode-webview origin; the HMR socket
    // must point back at this dev server explicitly.
    client: {
      host: 'localhost',
      protocol: 'ws',
    },
  },
}));
