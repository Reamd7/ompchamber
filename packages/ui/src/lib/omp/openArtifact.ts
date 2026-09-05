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

const fetchOmpArtifactBlob = async (url: string, sessionID: string, directory: string): Promise<Blob | null> => {
  const resolved = await uriApi.resolve({ url, sessionID, directory });
  if (!resolved.ok) return null;
  const { resource } = resolved;
  if (resource.content !== undefined) {
    return new Blob([resource.content], { type: resource.contentType || 'text/plain' });
  }
  if (resource.token) {
    const content = await uriApi.fetchContent({ token: resource.token.id, directory });
    if (content.ok) return content.blob;
  }
  return null;
};

export const openOmpArtifact = async (url: string, sessionID: string, directory: string): Promise<boolean> => {
  const blob = await fetchOmpArtifactBlob(url, sessionID, directory);
  if (!blob) return false;
  const objectUrl = URL.createObjectURL(blob);
  const opened = window.open(objectUrl, '_blank');
  if (!opened) URL.revokeObjectURL(objectUrl);
  return Boolean(opened);
};

/**
 * Fetch one artifact's text body (null on any failure or non-text payload) —
 * the inline counterpart of openOmpArtifact for rendering run output in the
 * task card instead of the spawn notice the async task tool settles with.
 */
export const fetchOmpArtifactText = async (url: string, sessionID: string, directory: string): Promise<string | null> => {
  const blob = await fetchOmpArtifactBlob(url, sessionID, directory);
  if (!blob) return null;
  const type = blob.type || 'text/plain';
  if (!type.startsWith('text/') && !type.includes('markdown') && !type.includes('json')) return null;
  const text = await blob.text();
  return text.trim().length > 0 ? text : null;
};
