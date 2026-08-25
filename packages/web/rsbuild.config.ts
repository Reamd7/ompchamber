import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineConfig, loadEnv, type RsbuildPlugin } from '@rsbuild/core';
import { pluginReact } from '@rsbuild/plugin-react';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const packageJson = JSON.parse(fs.readFileSync(path.resolve(__dirname, 'package.json'), 'utf-8'));
const themeDirectory = path.resolve(__dirname, '../ui/src/lib/theme/themes');
const reactScanToggle = (process.env.VITE_ENABLE_REACT_SCAN ?? '').toLowerCase();
const enableReactScan = reactScanToggle === '1' || reactScanToggle === 'true' || reactScanToggle === 'on' || reactScanToggle === 'yes';

// The client keeps the historical VITE_ prefix so existing deployments and the
// documented VITE_OPENCODE_URL contract stay valid without renaming.
const { publicVars } = loadEnv({ prefixes: ['VITE_'] });

/**
 * Live theme editing without a page reload. Built-in theme JSONs live in the
 * shared UI package; edits there are pushed to the dev server's clients as the
 * `openchamber:theme-updated` HMR event, which `src/main.tsx` forwards to the
 * theme system. The theme directory is excluded from Rspack's watcher so an
 * edit never falls back to a full rebuild + reload.
 */
const themeJsonHmrPlugin = (): RsbuildPlugin => ({
  name: 'openchamber-theme-json-hmr',
  setup(api) {
    let watcher: fs.FSWatcher | null = null;

    api.onBeforeStartDevServer(({ server }) => {
      watcher = fs.watch(themeDirectory, { recursive: true }, (_event, file) => {
        if (!file || path.extname(file) !== '.json') return;

        try {
          const theme = JSON.parse(fs.readFileSync(path.join(themeDirectory, file), 'utf-8'));
          server.environments.web.hot.send('custom', { event: 'openchamber:theme-updated', data: theme });
        } catch {
          // Leave the previous valid theme active while an editor writes
          // invalid or incomplete JSON; the next valid save replaces it.
        }
      });
    });

    api.onCloseDevServer(() => {
      watcher?.close();
      watcher = null;
    });
  },
});

const moduleResourceOf = (module: unknown): string | undefined => {
  if (typeof module === 'object' && module !== null && 'resource' in module) {
    const resource = module.resource;
    return typeof resource === 'string' ? resource : undefined;
  }
  return undefined;
};

const packageNameOf = (resource: string | undefined): string | null => {
  if (!resource || !resource.includes('node_modules')) return null;
  const segments = resource.split(/[\\/]node_modules[\\/]/);
  const last = segments[segments.length - 1];
  if (!last) return null;
  const parts = last.split(/[\\/]/);
  return last.startsWith('@') ? `${parts[0]}/${parts[1]}` : parts[0];
};

// These are dynamically imported one at a time by their registries (Shiki
// grammars/themes, CodeMirror legacy modes) or split by usage (@pierre/diffs:
// the eager tool renderer needs only its pure patch parser). Forcing them into
// a shared vendor chunk would make the first language request download every
// grammar (7.4 MB raw for @shikijs/langs). Keep them with their importing
// chunks so only what is used gets fetched.
const CHUNK_SPLIT_EXEMPT = new Set([
  '@shikijs/langs',
  '@shikijs/themes',
  '@codemirror/legacy-modes',
  '@pierre/diffs',
]);

const vendorChunkName = (pkg: string): string => {
  if (pkg === 'react' || pkg === 'react-dom') return 'vendor-react';
  if (pkg === 'zustand') return 'vendor-zustand';
  if (pkg.includes('remark') || pkg.includes('rehype') || pkg === 'react-markdown') return 'vendor-markdown';
  if (pkg.startsWith('@base-ui')) return 'vendor-base-ui';
  return `vendor-${pkg.replace(/^@/, '').replace(/\//g, '-')}`;
};

const themeDirectoryPattern = new RegExp(
  `[\\\\/]node_modules[\\\\/]|\\.git|${themeDirectory.replace(/[.*+?^${}()|[\]\\]/g, '\\$&').replace(/\\\\/g, '[\\\\/]')}`,
);


export default defineConfig(({ command }) => ({
  plugins: [
    pluginReact({
      reactCompiler: true,
      // The vendor cache group below already pins react/react-dom to a stable
      // `vendor-react` chunk; the plugin's extra lib-react split would only
      // duplicate that intent.
      splitChunks: false,
    }),
    themeJsonHmrPlugin(),
  ],
  environments: {
    web: {
      source: {
        entry: {
          index: './src/main.tsx',
          mobile: './src/mobile-main.tsx',
          'mini-chat': './src/mini-chat-main.tsx',
        },
      },
      output: {
        target: 'web',
        module: true,
        // vite emitted no sidecar license files; keep the dist layout clean.
        legalComments: 'none',
      },
      performance: {
        chunkSplit: { strategy: 'custom' },
      },
    },
    // The push-notification service worker. `web-worker` target emits a
    // classic (non-module) single-file bundle — iOS Safari/PWA is much more
    // reliable with a non-module SW — and skips HTML generation entirely.
    // Dev never serves or registers a SW (the app unregisters leftover
    // registrations in development), so it is only compiled for builds.
    ...(command !== 'dev' && {
      sw: {
        source: {
          entry: { sw: './src/sw.ts' },
        },
        output: {
          target: 'web-worker',
          distPath: { root: './dist', js: '.' },
          filename: { js: 'sw.js' },
          // The web environment owns cleaning the shared dist root.
          cleanDistPath: false,
        },
      },
    }),
  },
  resolve: {
    alias: {
      '@openchamber/ui': path.resolve(__dirname, '../ui/src'),
      '@web': path.resolve(__dirname, './src'),
      '@': path.resolve(__dirname, '../ui/src'),
    },
  },
  source: {
    define: {
      ...publicVars,
      __APP_VERSION__: JSON.stringify(packageJson.version),
      'process.env': '{}',
      global: 'globalThis',
    },
  },
  html: {
    template: ({ entryName }) => {
      const templates: Record<string, string> = {
        index: './index.html',
        mobile: './mobile.html',
        'mini-chat': './mini-chat.html',
      };
      return templates[entryName];
    },
    tags: [
      ...(command === 'dev'
        ? [
            {
              tag: 'script',
              // React 19.2 dev builds wrap every render/effect in a
              // console.createTask task; with DevTools attached that wrapper
              // dominated chat-switch main-thread time (8.2s task, multi-second
              // self time). react-dom captures the method when its vendor chunk
              // evaluates — before any app module — so the patch must run in
              // the document head. localStorage `oc-dev-console-tasks=1`
              // restores it.
              children:
                "try{if(localStorage.getItem('oc-dev-console-tasks')!=='1'&&'createTask' in console){Object.defineProperty(console,'createTask',{value:undefined,configurable:true})}}catch(e){}",
              head: true,
              append: false,
            },
          ]
        : []),
      ...(enableReactScan
        ? [
            {
              tag: 'script',
              attrs: { crossorigin: 'anonymous', src: '//unpkg.com/react-scan/dist/auto.global.js' },
              head: true,
              append: false,
            },
          ]
        : []),
    ],
  },
  server: {
    port: 5173,
    // changeOrigin must stay false (Rsbuild defaults it to true): the backend
    // derives trusted origins for WebSocket upgrades (and passkey RP origins)
    // from the Host/X-Forwarded-Host headers. Rewriting Host to the loopback
    // target makes every dev-proxy WebSocket upgrade (terminal/event/dictation)
    // fail the origin gate with 403 whenever the page is served from the dev
    // port, which is always the case in `bun run dev`.
    proxy: {
      '/auth': {
        target: `http://127.0.0.1:${process.env.OMPCHAMBER_PORT || 3001}`,
        changeOrigin: false,
      },
      '/health': {
        target: `http://127.0.0.1:${process.env.OMPCHAMBER_PORT || 3001}`,
        changeOrigin: false,
      },
      '/api': {
        target: `http://127.0.0.1:${process.env.OMPCHAMBER_PORT || 3001}`,
        changeOrigin: false,
        ws: true,
      },
    },
  },
  tools: {
    rspack: (rspackConfig, { environment }) => {
      if (environment.name !== 'web') return;

      rspackConfig.optimization ??= {};
      rspackConfig.optimization.splitChunks = {
        chunks: 'all',
        cacheGroups: {
          // One stable chunk per node_modules package (plus the pinned groups
          // above), mirroring the previous manualChunks split.
          vendor: {
            test: (module) => {
              const pkg = packageNameOf(moduleResourceOf(module));
              return pkg !== null && !CHUNK_SPLIT_EXEMPT.has(pkg);
            },
            name: (module) => {
              const pkg = packageNameOf(moduleResourceOf(module)) ?? 'misc';
              return vendorChunkName(pkg);
            },
            chunks: 'all',
            enforce: true,
            priority: 10,
          },
        },
      };
      rspackConfig.module ??= {};
      rspackConfig.module.rules = [
        ...(rspackConfig.module.rules ?? []),
        {
          // @pierre/diffs declares `sideEffects: ["dist/components/web-components.js"]`,
          // so its worker entry — a pure side-effect module (top-level
          // self.addEventListener, zero exports) — tree-shakes away when the
          // app's worker entry imports it for side effects only. The emitted
          // worker chunk is then empty: the worker spawns, answers nothing,
          // and every diff render hangs silently with blank output.
          test: /[\\/]node_modules[\\/]@pierre[\\/]diffs[\\/]dist[\\/]worker[\\/]worker\.js$/,
          sideEffects: true,
        },
      ];


      // Theme JSON edits are handled by the HMR event above; keep them (and
      // the default ignores) out of Rspack's watcher so they never trigger a
      // rebuild + full reload.
      rspackConfig.watchOptions = {
        ignored: themeDirectoryPattern,
      };
    },
  },
}));
