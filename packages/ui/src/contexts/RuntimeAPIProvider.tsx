import React, { type JSX, type ReactNode } from 'react';
import { RuntimeAPIContext } from '@/contexts/runtimeAPIContext';
import type { FilesAPI, RuntimeAPIs } from '@/lib/api/types';
import { createContentCachedFiles } from '@/contexts/content-cache-owner';

type ContentCachedFiles = ReturnType<typeof createContentCachedFiles>;

export function RuntimeAPIProvider({ apis, children }: { apis: RuntimeAPIs; children: ReactNode }): JSX.Element {
  // Effect-owned lifecycle: React Strict Mode dispose+remount must create a fresh
  // owner. useMemo + dispose reused a dead owner and broke text-file opens
  // (binaries skipped the pre-read, so they still appeared to work).
  // Lazy state initializer so the first committed render already exposes the
  // owner's `files` — creating it in an effect instead leaves the first
  // render on the raw `apis.files`, then flips identity on mount and churns
  // every consumer (including effects that re-fire on the swap).
  const [cachedOwner, setCachedOwner] = React.useState<ContentCachedFiles | null>(
    () => createContentCachedFiles(apis.files),
  );

  // Adopt a new owner only when `apis.files` actually changes identity
  // (runtime/endpoint switch). Compared against the owner currently held in
  // state, so the effect is a no-op on mount and on every re-render — the
  // lazy initializer already supplied the right owner. Strict Mode's
  // dispose+remount still works: cleanup nulls the owner, so the next
  // effect pass creates a fresh one.
  const currentFiles = cachedOwner?.files ?? apis.files;
  React.useEffect(() => {
    if (currentFiles === apis.files) return;
    const owner = createContentCachedFiles(apis.files);
    setCachedOwner((current) => {
      current?.dispose();
      return owner;
    });
    return () => {
      owner.dispose();
      setCachedOwner((current) => (current === owner ? null : current));
    };
  }, [apis.files, currentFiles]);

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
