import { createOmpUriAPI } from '@/lib/api/omp';

/**
 * Open an `agent://` / `local://` artifact URL through the omp URI bridge
 * (spec 04 §5.2): resolve mints a resource token (session-pinned — the
 * caller passes the owning session), the text body rides the resolve
 * response and binaries come back as bytes via the token content endpoint;
 * the result opens as an object URL in a new tab. Returns false on any
 * failure — callers surface their own inline error state.
 */
const uriApi = createOmpUriAPI();

export const openOmpArtifact = async (url: string, sessionID: string, directory: string): Promise<boolean> => {
  const resolved = await uriApi.resolve({ url, sessionID, directory });
  if (!resolved.ok) return false;
  const { resource } = resolved;
  let blob: Blob | null = null;
  if (resource.content !== undefined) {
    blob = new Blob([resource.content], { type: resource.contentType || 'text/plain' });
  } else if (resource.token) {
    const content = await uriApi.fetchContent({ token: resource.token.id, directory });
    if (content.ok) blob = content.blob;
  }
  if (!blob) return false;
  const objectUrl = URL.createObjectURL(blob);
  const opened = window.open(objectUrl, '_blank');
  if (!opened) URL.revokeObjectURL(objectUrl);
  return Boolean(opened);
};
