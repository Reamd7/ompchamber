import { createConfiguredWebAPIs, getDesktopRelayRestoreReady } from './runtimeConfig';
import { Workbox } from 'workbox-window';

import type { RuntimeAPIs } from '@openchamber/ui/lib/api/types';
import { resolveHostedSurface, type HostedSurface } from '@openchamber/ui/lib/runtimeSurface';
import {
  isEmbeddedSessionChat,
  requestEmbeddedSessionRuntimeBootstrap,
} from '@openchamber/ui/components/layout/contextPanelEmbeddedChat';
import '@openchamber/ui/index.css';
import '@openchamber/ui/styles/fonts';

declare global {
  interface Window {
    __OPENCHAMBER_RUNTIME_APIS__?: RuntimeAPIs;
    __OPENCHAMBER_SURFACE__?: HostedSurface;
  }
}

const hostedSurface: HostedSurface = resolveHostedSurface();

type PrerenderingDocument = Document & {
  prerendering?: boolean;
};

const canUseServiceWorker = (): boolean => {
  if (!('serviceWorker' in navigator)) return false;
  if (!window.isSecureContext) return false;
  if (window.location.protocol !== 'http:' && window.location.protocol !== 'https:') return false;

  const documentState = document as PrerenderingDocument;
  if (documentState.prerendering || String(document.visibilityState) === 'prerender') {
    return false;
  }

  return true;
};

const runWhenDocumentCanRegisterServiceWorker = (task: () => void): void => {
  let completed = false;
  const run = () => {
    if (completed) return;
    if (canUseServiceWorker()) {
      completed = true;
      task();
    }
  };

  const afterLoad = () => {
    setTimeout(run, 0);
  };

  if (document.readyState === 'complete') {
    afterLoad();
  } else {
    window.addEventListener('load', afterLoad, { once: true });
  }

  const documentState = document as PrerenderingDocument;
  if (documentState.prerendering || String(document.visibilityState) === 'prerender') {
    document.addEventListener('visibilitychange', run, { once: true });
  }
};

const registerPwaServiceWorker = (): void => {
  runWhenDocumentCanRegisterServiceWorker(() => {
    try {
      // workbox-window is what vite-plugin-pwa's registerSW wrapped; using it
      // directly keeps the autoUpdate registration semantics without the
      // virtual module.
      const wb = new Workbox(`${import.meta.env.BASE_URL}sw.js`);
      wb.register().catch((error: unknown) => {
        console.warn('[PWA] service worker registration skipped:', error);
      });
    } catch (error) {
      console.warn('[PWA] service worker registration skipped:', error);
    }
  });
};

const unregisterDevelopmentServiceWorkers = (): void => {
  runWhenDocumentCanRegisterServiceWorker(() => {
    void navigator.serviceWorker.getRegistrations()
      .then((registrations) => Promise.all(registrations.map((registration) => registration.unregister())))
      .catch(() => {});
  });
};

const start = async (): Promise<void> => {
  const embeddedBootstrap = isEmbeddedSessionChat()
    ? await requestEmbeddedSessionRuntimeBootstrap()
    : null;
  window.__OPENCHAMBER_RUNTIME_APIS__ = createConfiguredWebAPIs(embeddedBootstrap);

  if (hostedSurface === 'mobile') {
    const { renderMobileApp } = await import('@openchamber/ui/apps/renderMobileApp');
    renderMobileApp(window.__OPENCHAMBER_RUNTIME_APIS__);
    return;
  }

  // Hold the render until a desktop relay-host restore has picked its transport.
  await getDesktopRelayRestoreReady();
  await import('@openchamber/ui/main');
};

void start();

// Built-in theme JSON edits arrive as a custom rsbuild HMR event pushed by
// the dev server plugin; forward them to the theme system without a reload.
// `module.hot.on` only exists when the rsbuild HMR client has injected its
// interceptor — in production (and in dev before that runs) calling `.on`
// throws a TypeError that also swallowed the PWA registration below, so
// check for the function itself, never just the hot object.
if (import.meta.env.DEV && typeof import.meta.webpackHot?.on === 'function') {
  import.meta.webpackHot.on('openchamber:theme-updated', (theme: unknown) => {
    window.dispatchEvent(new CustomEvent('openchamber:theme-hmr', { detail: theme }));
  });
}

if (import.meta.env.PROD) {
  registerPwaServiceWorker();
} else {
  unregisterDevelopmentServiceWorkers();
}
