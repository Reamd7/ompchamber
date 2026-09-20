import React, { type JSX, type ReactNode } from 'react';
import { RuntimeAPIContext } from '@/contexts/runtimeAPIContext';
import type { FilesAPI, RuntimeAPIs } from '@/lib/api/types';
import { createContentCachedFiles, type ContentCachedFiles } from '@/contexts/content-cache-owner';

export function RuntimeAPIProvider({ apis, children }: { apis: RuntimeAPIs; children: ReactNode }): JSX.Element {
  // Lazy state initializer so the first committed render already exposes the
  // owner's `files` — creating it in an effect instead leaves the first
  // render on the raw `apis.files`, then flips identity on mount and churns
  // every consumer (including effects that re-fire on the swap).
  const [cachedOwner, setCachedOwner] = React.useState<ContentCachedFiles | null>(
    () => createContentCachedFiles(apis.files),
  );
  // Mirror of the live owner so this effect's cleanup can dispose it without
  // reading it out of state. Reading it from state means depending on it, and
  // that dependency is what made this effect re-trigger itself (below).
  const liveOwnerRef = React.useRef<ContentCachedFiles | null>(cachedOwner);

  // Effect-owned lifecycle: React Strict Mode dispose+remount must create a
  // fresh owner. useMemo + dispose reused a dead owner and broke text-file
  // opens (binaries skipped the pre-read, so they still appeared to work).
  //
  // `apis.files` identity is the only trigger AND the only dependency. The
  // previous version also listed the owner's own `files`: adopting an owner
  // changed that dependency, so the effect re-ran, disposed the owner it had
  // just adopted, adopted another one, and never settled — "Maximum update
  // depth exceeded" on mount, with every subsequent effect churn cascading
  // into consumer updates.
  //
  // Adoption matches on `source`: an owner wraps the source files API, so its
  // `files` wrapper is never identical to `apis.files`.
  React.useEffect(() => {
    const inherited = liveOwnerRef.current;
    const owner = inherited?.source === apis.files ? inherited : createContentCachedFiles(apis.files);
    if (owner !== inherited) inherited?.dispose();
    liveOwnerRef.current = owner;
    setCachedOwner(owner);
    return () => {
      // Dropping the owner instead of reusing it is what gives Strict Mode's
      // remount a live owner: the next pass finds nothing to inherit.
      if (liveOwnerRef.current === owner) liveOwnerRef.current = null;
      owner.dispose();
      setCachedOwner((current) => (current === owner ? null : current));
    };
  }, [apis.files]);

  const files: FilesAPI = cachedOwner?.files ?? apis.files;
  const cachedApis = React.useMemo<RuntimeAPIs>(
    () => ({
      ...apis,
      files,
    }),
    [apis, files],
  );
  return <RuntimeAPIContext.Provider value={cachedApis}>{children}</RuntimeAPIContext.Provider>;
}
