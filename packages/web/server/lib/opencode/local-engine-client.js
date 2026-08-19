// Server-side HTTP client for the managed omp host.
//
// Replaces the @opencode-ai/sdk generated client in the web server's own
// call sites (openchamber-sessions, scheduled-tasks, openchamber-control,
// skill-routes). Same result convention as the SDK: every call resolves to
// `{ data, error?, response }` and never throws for HTTP outcomes.
//
// Node-safe (no Bun or TypeScript imports) — the web server also runs
// in-process inside Electron's Node runtime.

const asQuery = (params) => {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params ?? {})) {
    if (value === undefined || value === null) continue;
    query.set(key, String(value));
  }
  const text = query.toString();
  return text ? `?${text}` : '';
};

export const createLocalEngineClient = ({ baseUrl, headers = {}, directory, fetchImpl = fetch } = {}) => {
  const root = String(baseUrl ?? '').replace(/\/+$/, '');
  if (!root) throw new Error('createLocalEngineClient requires a baseUrl');
  const baseHeaders = {
    ...headers,
    ...(directory ? { 'x-opencode-directory': encodeURIComponent(directory) } : {}),
  };

  const request = async (method, pathname, { query, body } = {}) => {
    try {
      const response = await fetchImpl(`${root}${pathname}${asQuery(query)}`, {
        method,
        headers: {
          ...(body !== undefined ? { 'content-type': 'application/json' } : {}),
          ...baseHeaders,
        },
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
      });
      const text = await response.text();
      let payload = null;
      if (text) {
        try {
          payload = JSON.parse(text);
        } catch {
          payload = { message: text };
        }
      }
      if (!response.ok) {
        const message = payload?.data?.message ?? payload?.message ?? `${method} ${pathname} failed with ${response.status}`;
        return { data: undefined, error: { name: payload?.name ?? 'UnknownError', message }, response };
      }
      return { data: payload, error: undefined, response };
    } catch (error) {
      return { data: undefined, error: { name: 'UnknownError', message: error?.message ?? String(error) }, response: undefined };
    }
  };

  return {
    session: {
      create: (params) => request('POST', '/session', { body: params }),
      list: (params) => request('GET', '/session', { query: params }),
      status: (params) => request('GET', '/session/status', { query: params }),
      messages: ({ sessionID, ...rest }) =>
        request('GET', `/session/${encodeURIComponent(sessionID)}/message`, { query: rest }),
      command: ({ sessionID, ...rest }) =>
        request('POST', `/session/${encodeURIComponent(sessionID)}/command`, { body: rest }),
      fork: ({ sessionID, ...rest }) =>
        request('POST', `/session/${encodeURIComponent(sessionID)}/fork`, { body: rest }),
    },
    experimental: {
      session: {
        list: (params) => request('GET', '/experimental/session', { query: params }),
      },
    },
    command: {
      list: (params) => request('GET', '/command', { query: params }),
    },
    app: {
      skills: (params) => request('GET', '/skill', { query: params }),
    },
  };
};
