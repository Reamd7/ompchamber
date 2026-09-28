/**
 * proxy-headers 模块的单元测试套件（vitest）。
 *
 * 验证 OpenCode 反向代理的 header 白名单行为：客户端凭证与
 * hop-by-hop header 不被转发、受管上游鉴权正确注入、上游响应中
 * content-encoding / transfer-encoding 等被剔除而普通 header 保留。
 */
import { describe, expect, it } from 'vitest';

import {
  applyForwardProxyResponseHeaders,
  collectForwardProxyHeaders,
  shouldForwardProxyResponseHeader,
} from './proxy-headers.js';

/** 覆盖转发请求 header 的组装规则：剔除白名单键、注入受管鉴权。 */
describe('OpenCode proxy header handling', () => {
  it('drops accept-encoding from forwarded request headers', () => {
    const headers = collectForwardProxyHeaders({
      accept: 'application/json',
      'accept-encoding': 'gzip, deflate, br',
      connection: 'keep-alive',
    });

    expect(headers.accept).toBe('application/json');
    expect(headers['accept-encoding']).toBeUndefined();
  });

  it('replaces client authorization with managed OpenCode auth', () => {
    const headers = collectForwardProxyHeaders(
      { authorization: 'Bearer oc_client_stale-ui-token' },
      { Authorization: 'Bearer managed-opencode-token' },
    );

    expect(headers.Authorization).toBe('Bearer managed-opencode-token');
    expect(headers['authorization']).toBeUndefined();
  });

  it('drops client authorization when upstream has no managed auth', () => {
    const headers = collectForwardProxyHeaders({
      accept: 'application/json',
      authorization: 'Bearer oc_client_stale-ui-token',
    });

    expect(headers['authorization']).toBeUndefined();
    expect(headers.Authorization).toBeUndefined();
    expect(headers.accept).toBe('application/json');
  });

  it('drops content-encoding from forwarded response headers', () => {
    expect(shouldForwardProxyResponseHeader('content-encoding')).toBe(false);
    expect(shouldForwardProxyResponseHeader('Content-Encoding')).toBe(false);
  });

  it('drops transfer-encoding from forwarded response headers', () => {
    expect(shouldForwardProxyResponseHeader('transfer-encoding')).toBe(false);
    expect(shouldForwardProxyResponseHeader('Transfer-Encoding')).toBe(false);
  });

  it('still keeps ordinary response headers', () => {
    expect(shouldForwardProxyResponseHeader('content-type')).toBe(true);
    expect(shouldForwardProxyResponseHeader('etag')).toBe(true);
  });

  it('applies upstream response headers to express response without content-encoding', () => {
    const applied = [];
    const response = {
      setHeader(key, value) {
        applied.push([key, value]);
      },
    };

    applyForwardProxyResponseHeaders(
      new Headers({
        'content-type': 'application/json',
        etag: 'W/"abc"',
        'content-encoding': 'gzip',
      }),
      response,
    );

    expect(applied).toEqual([
      ['content-type', 'application/json'],
      ['etag', 'W/"abc"'],
    ]);
  });
});
