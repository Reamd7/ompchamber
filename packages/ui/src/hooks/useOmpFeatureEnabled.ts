/**
 * useOmpFeatureEnabled — reactive read for one `dialogs.v1`-style capability
 * key (master D6-R2: capabilities are server-adjudicated; the UI never keeps
 * a local feature flag).
 *
 * Subscribes to the shared per-runtime probe: false until it settles (legacy
 * behavior), then the server's answer. A runtime endpoint switch re-probes
 * under the new identity.
 */

import { useEffect, useState } from 'react';
import { isOmpFeatureEnabled, primeOmpCapabilityGate } from '@/lib/omp/capabilityGate';
import { getRuntimeKey, subscribeRuntimeEndpointChanged } from '@/lib/runtime-switch';

export const useOmpFeatureEnabled = (featureKey: string): boolean => {
  const [runtimeKey, setRuntimeKey] = useState(() => getRuntimeKey());
  const [enabled, setEnabled] = useState(() => isOmpFeatureEnabled(featureKey));

  useEffect(() => {
    let cancelled = false;
    const evaluate = (): void => {
      if (!cancelled) setEnabled(isOmpFeatureEnabled(featureKey));
    };
    evaluate();
    void primeOmpCapabilityGate().then(evaluate, evaluate);
    const stop = subscribeRuntimeEndpointChanged(() => {
      setRuntimeKey(getRuntimeKey());
    });
    return () => {
      cancelled = true;
      stop();
    };
    // runtimeKey drives a re-probe under the new identity.
  }, [featureKey, runtimeKey]);

  return enabled;
};
