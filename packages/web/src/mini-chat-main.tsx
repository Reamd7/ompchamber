import { createConfiguredWebAPIs } from './runtimeConfig';
import type { RuntimeAPIs } from '@openchamber/ui/lib/api/types';
import '@openchamber/ui/index.css';
import '@openchamber/ui/styles/fonts';

declare global {
  interface Window {
    __OMPCHAMBER_RUNTIME_APIS__?: RuntimeAPIs;
  }
}

window.__OMPCHAMBER_RUNTIME_APIS__ = createConfiguredWebAPIs();

void import('@openchamber/ui/apps/renderElectronMiniChatApp')
  .then(({ renderElectronMiniChatApp }) => {
    renderElectronMiniChatApp(window.__OMPCHAMBER_RUNTIME_APIS__ ?? createConfiguredWebAPIs());
  });
