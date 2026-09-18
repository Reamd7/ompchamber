import * as vscode from 'vscode';
import * as os from 'os';
import { getThemeKindName } from './theme';
import type { ConnectionStatus } from './opencode';
import type { WorkspaceFolderCandidate } from './workspaceResolver';

type PanelType = 'chat' | 'agentManager';

export interface WebviewHtmlOptions {
  webview: vscode.Webview;
  extensionUri: vscode.Uri;
  workspaceFolder: string;
  workspaceFolders?: WorkspaceFolderCandidate[];
  initialStatus: ConnectionStatus;
  cliAvailable: boolean;
  panelType?: PanelType;
  initialSessionId?: string;
  viewMode?: 'sidebar' | 'editor';
  devServerUrl?: string | null;
  extensionVersion?: string;
}

const asCspToken = (value: string | null | undefined): string | null => {
  if (!value) {
    return null;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

const toOrigin = (value: string | null | undefined): string | null => {
  if (!value) {
    return null;
  }
  try {
    return new URL(value).origin;
  } catch {
    return null;
  }
};

const uniqueTokens = (values: Array<string | null | undefined>): string => {
  return Array.from(new Set(values.map(asCspToken).filter((value): value is string => Boolean(value)))).join(' ');
};

export function getWebviewHtml(options: WebviewHtmlOptions): string {
  const {
    webview,
    extensionUri,
    workspaceFolder,
    workspaceFolders = [],
    initialStatus,
    cliAvailable,
    panelType = 'chat',
    initialSessionId,
    viewMode = 'sidebar',
    devServerUrl,
    extensionVersion = '',
  } = options;
  const workspaceFoldersJson = JSON.stringify(workspaceFolders).replace(/</g, '\\u003c');

  const scriptPath = vscode.Uri.joinPath(extensionUri, 'dist', 'webview', 'assets', 'index.js');
  const scriptUri = webview.asWebviewUri(scriptPath);
  const normalizedDevServerUrl = asCspToken(devServerUrl)?.replace(/\/$/, '') ?? null;
  const devServerOrigin = toOrigin(normalizedDevServerUrl);
  const styleSrc = uniqueTokens([webview.cspSource, "'unsafe-inline'", devServerOrigin]);
  const scriptSrc = uniqueTokens([webview.cspSource, "'unsafe-inline'", "'unsafe-eval'", devServerOrigin]);
  const connectSrc = uniqueTokens(['*', 'ws:', 'wss:', 'http:', 'https:', devServerOrigin]);
  const imgSrc = uniqueTokens([webview.cspSource, 'data:', 'https:', devServerOrigin]);
  const fontSrc = uniqueTokens([webview.cspSource, 'data:', devServerOrigin]);
  // fflate's async browser inflater creates blob-backed workers. Keep blob:
  // scoped to worker-src so document decompression works without allowing blob scripts.
  const workerSrc = uniqueTokens([webview.cspSource, 'blob:', devServerOrigin]);

  const themeKind = getThemeKindName(vscode.window.activeColorTheme.kind);

  // Use VS Code CSS variables for proper theme integration
  // These variables are automatically provided by VS Code to webviews
  // 
  // Logo mark matches OMPChamberLogo.tsx: the official Oh My Pi π with the orange
  // plugin connector, centered in the 100×100 view box.
  return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${styleSrc}; script-src ${scriptSrc}; connect-src ${connectSrc}; img-src ${imgSrc}; font-src ${fontSrc}; worker-src ${workerSrc};">
  <style>
    html, body, #root { height: 100%; width: 100%; margin: 0; padding: 0; }
    body { 
      overflow: hidden; 
      background: var(--vscode-editor-background, var(--vscode-sideBar-background)); 
      font-family: var(--vscode-font-family, -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif);
      color: var(--vscode-foreground);
    }
    
    /* Initial loading screen styles - uses VS Code theme variables */
    #initial-loading {
      position: fixed;
      inset: 0;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      gap: 16px;
      z-index: 9999;
      background: var(--vscode-editor-background, var(--vscode-sideBar-background));
      transition: opacity 0.3s ease-out;
    }
    #initial-loading.fade-out {
      opacity: 0;
      pointer-events: none;
    }
    /* Glow pulse on the Oh My Pi mark — signals loading without text. */
    @keyframes oc-logo-glow {
      0%, 100% { filter: drop-shadow(0 0 0 transparent); }
      50% { filter: drop-shadow(0 0 4px var(--vscode-foreground)); }
    }
    #initial-loading .logo {
      animation: oc-logo-glow 1.8s ease-in-out infinite;
    }
    @media (prefers-reduced-motion: reduce) {
      #initial-loading .logo { animation: none; }
    }
    /* Logo colors use VS Code foreground color */
    #initial-loading .logo-fill-solid {
      fill: var(--vscode-foreground);
    }
    #initial-loading .status-text {
      font-size: 13px;
      color: var(--vscode-descriptionForeground, var(--vscode-foreground));
      text-align: center;
    }
    #initial-loading .error-text {
      font-size: 12px;
      color: var(--vscode-errorForeground, #f48771);
      text-align: center;
      max-width: 280px;
    }
  </style>
  <title>OMPChamber</title>
</head>
<body>
  <!-- Initial loading screen with the official Oh My Pi mark -->
  <div id="initial-loading">
    <svg class="logo" width="70" height="70" viewBox="0 0 100 100" fill="none" xmlns="http://www.w3.org/2000/svg">
      <!-- Official Oh My Pi mark: π bar + two legs + orange plugin connector -->
      <g transform="translate(-2.8 10.4) scale(0.88)">
        <rect class="logo-fill-solid" x="10" y="8" width="100" height="12" rx="2"/>
        <rect class="logo-fill-solid" x="25" y="20" width="12" height="62" rx="2"/>
        <rect class="logo-fill-solid" x="75" y="20" width="12" height="45" rx="2"/>
        <path fill-rule="evenodd" clip-rule="evenodd" d="M74 55 H88 A3 3 0 0 1 91 58 V68 A3 3 0 0 1 88 71 H74 A3 3 0 0 1 71 68 V58 A3 3 0 0 1 74 55 Z M76 59 h3 v8 h-3 Z M82 59 h3 v8 h-3 Z" fill="#f97316"/>
        <circle cx="18" cy="14" r="2" fill="#f97316" opacity="0.8"/>
        <circle cx="102" cy="14" r="2" fill="#f97316" opacity="0.8"/>
      </g>
    </svg>
    <!-- Status text stays empty while things are fine; populated only on error. -->
    <div class="status-text" id="loading-status"></div>
    ${!cliAvailable ? `<div class="error-text" id="cli-missing-text">The engine failed to start.</div>` : ''}
  </div>
  
  <div id="root"></div>
  <script>
    // Polyfill process for Node.js modules running in browser
    window.process = window.process || { env: { NODE_ENV: 'production' }, platform: '', version: '', browser: true };

    window.__VSCODE_CONFIG__ = {
      workspaceFolder: "${workspaceFolder.replace(/\\/g, '\\\\')}",
      workspaceFolders: ${workspaceFoldersJson},
      theme: "${themeKind}",
      connectionStatus: "${initialStatus}",
      cliAvailable: ${cliAvailable},
      extensionVersion: "${extensionVersion.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}",
      platform: "${os.platform()}",
      arch: "${os.arch()}",
      panelType: "${panelType}",
      viewMode: "${viewMode}",
      initialSessionId: ${initialSessionId ? `"${initialSessionId.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"` : 'null'},
    };
    window.__OMPCHAMBER_HOME__ = "${workspaceFolder.replace(/\\/g, '\\\\')}";
    
    function getBootstrapMessages() {
      var locale = 'en';
      try {
        var rawLocale = window.localStorage.getItem('ompchamber.i18n.v1');
        if (rawLocale) {
          var parsedLocale = JSON.parse(rawLocale);
          if (parsedLocale && typeof parsedLocale.locale === 'string' && parsedLocale.locale.toLowerCase().indexOf('fr') === 0) {
            locale = 'fr';
          }
        }
      } catch {}

      return locale === 'fr'
        ? {
            startingApi: 'Démarrage du moteur…',
            initializing: 'Initialisation…',
            connecting: 'Connexion…',
            connected: 'Connecté !',
            connectionError: 'Erreur de connexion',
            reconnecting: 'Reconnexion…',
            cliNotFound: 'Le démarrage du moteur a échoué.',
          }
        : {
            startingApi: 'Starting the engine…',
            initializing: 'Initializing…',
            connecting: 'Connecting…',
            connected: 'Connected!',
            connectionError: 'Connection error',
            reconnecting: 'Reconnecting…',
            cliNotFound: 'The engine failed to start.',
          };
    }

    (function applyBootstrapLocale() {
      var statusEl = document.getElementById('loading-status');
      var cliMissingEl = document.getElementById('cli-missing-text');
      var messages = getBootstrapMessages();
      if (cliMissingEl) {
        cliMissingEl.textContent = messages.cliNotFound;
      }
      if (statusEl) {
        statusEl.textContent = '';
      }
    })();

    // Handle connection status updates to update loading screen
    window.addEventListener('message', function(event) {
      var msg = event.data;
      if (msg && msg.type === 'connectionStatus') {
        var messages = getBootstrapMessages();
        var statusEl = document.getElementById('loading-status');
        if (statusEl) {
          // Only show text when something is wrong — progress states stay silent
          // (the animated logo already signals "working").
          if (msg.status === 'error') {
            statusEl.textContent = msg.error || messages.connectionError;
            statusEl.classList.add('error-text');
          } else {
            statusEl.textContent = '';
            statusEl.classList.remove('error-text');
          }
        }
      }
    });
  </script>
  <script type="module">
    const prodEntryUrl = ${JSON.stringify(scriptUri.toString())};
    const devServerUrl = ${normalizedDevServerUrl ? JSON.stringify(normalizedDevServerUrl) : 'null'};

    const loadProductionBundle = () => {
      const script = document.createElement('script');
      script.type = 'module';
      script.src = prodEntryUrl;
      document.body.appendChild(script);
    };

    if (!devServerUrl) {
      loadProductionBundle();
    } else {
      const baseUrl = devServerUrl;

      const statusEl = document.getElementById('loading-status');
      const getDevMessages = () => {
        try {
          const rawLocale = window.localStorage.getItem('ompchamber.i18n.v1');
          if (rawLocale) {
            const parsedLocale = JSON.parse(rawLocale);
            if (parsedLocale && typeof parsedLocale.locale === 'string' && parsedLocale.locale.toLowerCase().indexOf('fr') === 0) {
              return {
                startingDevServer: (host) => 'Démarrage du serveur de développement de la webview (' + host + ')...',
                waitingDevServer: (host, attempt) => 'En attente du serveur de développement de la webview (' + host + ')... tentative ' + attempt,
              };
            }
          }
        } catch {}
        return {
          startingDevServer: (host) => 'Starting webview dev server (' + host + ')...',
          waitingDevServer: (host, attempt) => 'Waiting for webview dev server (' + host + ')... attempt ' + attempt,
        };
      };
      const setStatus = (text) => {
        if (statusEl) {
          statusEl.textContent = text;
        }
      };

      const retryDelayMs = 500;
      let attempt = 0;

      const waitForRootMount = (timeoutMs) => {
        const root = document.getElementById('root');
        if (!root) {
          return Promise.resolve(false);
        }

        if (root.childNodes.length > 0) {
          return Promise.resolve(true);
        }

        return new Promise((resolve) => {
          const observer = new MutationObserver(() => {
            if (root.childNodes.length > 0) {
              observer.disconnect();
              clearTimeout(timer);
              resolve(true);
            }
          });

          observer.observe(root, { childList: true, subtree: true });
          const timer = window.setTimeout(() => {
            observer.disconnect();
            resolve(root.childNodes.length > 0);
          }, timeoutMs);
        });
      };

      const tryLoadDevBundle = () => {
        // The rsbuild dev server serves the compiled webview entry at a
        // stable unhashed path (rsbuild.config.ts pins output.filename to
        // assets/[name].js), with the HMR client bundled into it.
        const devEntryUrl = baseUrl + '/assets/index.js';
        const hostLabel = (() => {
          try {
            return new URL(baseUrl).host;
          } catch {
            return baseUrl;
          }
        })();

        const devMessages = getDevMessages();
        setStatus(devMessages.startingDevServer(hostLabel));

        Promise.resolve()
          .then(() => import(devEntryUrl))
          .then(() => waitForRootMount(4000))
          .then((mounted) => {
            if (!mounted) {
              throw new Error('Dev bundle loaded but app did not mount');
            }
          })
          .catch((error) => {
            attempt += 1;
            console.warn('[OMPChamber] VS Code webview dev bundle unavailable, retrying...', error);
            setStatus(devMessages.waitingDevServer(hostLabel, attempt));
            window.setTimeout(() => {
              tryLoadDevBundle();
            }, retryDelayMs);
          });
      };


      tryLoadDevBundle();
    }
  </script>
</body>
</html>`;
}
