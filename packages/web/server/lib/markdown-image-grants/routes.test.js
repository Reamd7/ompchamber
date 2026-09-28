/**
 * markdown-image-grants 路由（会话图片授权）的集成测试套件。
 *
 * 用 supertest 驱动真实 express 应用与临时目录中的真实文件，stub 全局 fetch
 * 模拟 OpenCode 消息接口。覆盖：目录头的 percent-encode、工作区与 OpenCode
 * 临时目录图片的一次消息拉取与授权下发、部分失败语义、引用式/带括号/带 title
 * 等 Markdown 语法解析，以及对越界路径、符号链接逃逸和非图片字节的拒绝。
 */
import { afterEach, describe, expect, it, vi } from 'vitest';
import express from 'express';
import request from 'supertest';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { registerMarkdownImageGrantRoutes } from './routes.js';

/** 1x1 像素的合法 PNG 字节，作为测试夹具中的图片文件内容。 */
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==',
  'base64',
);
/** 每个用例创建的临时根目录列表，由 afterEach 统一递归清理。 */
const roots = [];

/** 恢复被 stub 的全局变量，并删除本用例创建的全部临时目录。 */
afterEach(async () => {
  vi.unstubAllGlobals();
  await Promise.all(roots.splice(0).map((root) => fs.rm(root, { recursive: true, force: true })));
});

/**
 * 搭建完整测试夹具：创建临时根目录（含 approvedTempRoot 与 workspace 两个子目录）、
 * 写入默认图片文件，按 sources/markdown 生成助手消息文本并 stub 全局 fetch 返回该消息，
 * 最后向真实 express 应用注册被测路由。
 * readFile 包了一层计数器（fullReadCount），用于断言实现只读文件头而不整读文件。
 * 返回 app、各目录路径、fetchMock、fullReadCount 与请求所用 sources。
 */
const createFixture = async ({ sources, markdown } = {}) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'ompchamber-session-assets-'));
  roots.push(root);
  const approvedTempRoot = path.join(root, 'opencode');
  const directory = path.join(root, 'workspace');
  await Promise.all([
    fs.mkdir(approvedTempRoot, { recursive: true }),
    fs.mkdir(directory, { recursive: true }),
  ]);
  const defaultPath = path.join(approvedTempRoot, 'image.png');
  await fs.writeFile(defaultPath, PNG);
  const requestedSources = sources ?? [new URL(`file://${defaultPath}`).toString()];
  const text = markdown ?? requestedSources.map((source) => `![image](${source})`).join('\n');
  const fetchMock = vi.fn(async () => new Response(JSON.stringify({
    info: { id: 'msg_1', role: 'assistant' },
    parts: [{ type: 'text', text }],
  }), { status: 200, headers: { 'content-type': 'application/json' } }));
  vi.stubGlobal('fetch', fetchMock);

  let fullReadCount = 0;
  const app = express();
  registerMarkdownImageGrantRoutes(app, {
    fsPromises: {
      ...fs,
      readFile: async (...args) => {
        fullReadCount += 1;
        return fs.readFile(...args);
      },
    },
    path,
    os,
    crypto,
    approvedTempRoot,
    validateDirectoryPath: async (candidate) => candidate === directory
      ? { ok: true, directory }
      : { ok: false, error: 'Invalid directory' },
    buildOpenCodeUrl: (route) => `http://opencode.test${route}`,
    getOpenCodeAuthHeaders: () => ({ authorization: 'Basic test' }),
  });
  return {
    app,
    approvedTempRoot,
    directory,
    fetchMock,
    fullReadCount: () => fullReadCount,
    root,
    sources: requestedSources,
  };
};

/** 发起标准的图片授权 POST 请求（ses_1 会话、msg_1 消息），默认期望 200。 */
const prepare = (app, directory, sources) => request(app)
  .post('/api/ompchamber/sessions/ses_1/markdown-image-grants')
  .send({ directory, messageId: 'msg_1', sources })
  .expect(200);

/** 会话图片授权端到端行为：授权依据（助手文本）、Markdown 语法解析与文件安全边界。 */
describe('session image assets', () => {
  it('percent-encodes the directory header on the message fetch', async () => {
    const fixture = await createFixture();
    await prepare(fixture.app, fixture.directory, ['image.png']);

    expect(fixture.fetchMock).toHaveBeenCalledWith(
      expect.any(URL),
      expect.objectContaining({
        headers: expect.objectContaining({
          'x-opencode-directory': encodeURIComponent(fixture.directory),
        }),
      }),
    );
  });

  it('prepares workspace and OpenCode temporary images with one message fetch', async () => {
    const fixture = await createFixture({ sources: ['workspace.png'] });
    await fs.writeFile(path.join(fixture.directory, 'workspace.png'), PNG);
    const temporaryPath = path.join(fixture.approvedTempRoot, 'temporary.png');
    await fs.writeFile(temporaryPath, PNG);
    const temporarySource = new URL(`file://${temporaryPath}`).toString();
    fixture.fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({
      info: { id: 'msg_1', role: 'assistant' },
      parts: [{ type: 'text', text: `![workspace](workspace.png)\n![temporary](${temporarySource})` }],
    }), { status: 200, headers: { 'content-type': 'application/json' } }));

    const response = await prepare(fixture.app, fixture.directory, ['workspace.png', temporarySource]);

    expect(fixture.fetchMock).toHaveBeenCalledTimes(1);
    expect(fixture.fullReadCount()).toBe(0);
    expect(response.body.results).toHaveLength(2);
    const canonicalTemporaryPath = await fs.realpath(temporaryPath);
    expect(response.body.results[0]).toEqual({
      source: 'workspace.png',
      status: 'ready',
      path: path.join(fixture.directory, 'workspace.png'),
    });
    expect(response.body.results[1]).toEqual(expect.objectContaining({
      source: temporarySource,
      status: 'ready',
      path: canonicalTemporaryPath,
      outsideFileGrant: expect.any(String),
      expiresAt: expect.any(Number),
    }));
  });

  it('returns partial results without letting one missing image block valid images', async () => {
    const fixture = await createFixture({ sources: ['present.png', 'deleted.png'] });
    await fs.writeFile(path.join(fixture.directory, 'present.png'), PNG);

    const response = await prepare(fixture.app, fixture.directory, fixture.sources);

    expect(response.body.results).toEqual([
      expect.objectContaining({ source: 'present.png', status: 'ready' }),
      { source: 'deleted.png', status: 'missing' },
    ]);
  });

  it('resolves encoded workspace paths without treating query or fragment text as a filename', async () => {
    const source = 'screen%20shot.png?version=1#preview';
    const fixture = await createFixture({ sources: [source] });
    await fs.writeFile(path.join(fixture.directory, 'screen shot.png'), PNG);

    const response = await prepare(fixture.app, fixture.directory, fixture.sources);

    expect(response.body.results).toEqual([
      expect.objectContaining({ source, status: 'ready' }),
    ]);
  });

  it('authorizes reference-style image syntax using its resolved destination', async () => {
    const source = 'reference.png';
    const fixture = await createFixture({
      sources: [source],
      markdown: '![screenshot][result]\n\n[result]: reference.png',
    });
    await fs.writeFile(path.join(fixture.directory, source), PNG);

    const response = await prepare(fixture.app, fixture.directory, fixture.sources);

    expect(response.body.results).toEqual([
      expect.objectContaining({ source, status: 'ready' }),
    ]);
  });

  it('authorizes inline image destinations containing balanced parentheses', async () => {
    const source = 'screen(1).png';
    const fixture = await createFixture({
      sources: [source],
      markdown: `![screenshot](${source})`,
    });
    await fs.writeFile(path.join(fixture.directory, source), PNG);

    const response = await prepare(fixture.app, fixture.directory, fixture.sources);

    expect(response.body.results).toEqual([
      expect.objectContaining({ source, status: 'ready' }),
    ]);
  });

  it('requires inline image destinations with titles to close', async () => {
    const sources = ['valid.png', 'malformed.png'];
    const fixture = await createFixture({
      sources,
      markdown: '![valid](valid.png "preview")\n![malformed](malformed.png "preview"',
    });
    await Promise.all(sources.map((source) => fs.writeFile(path.join(fixture.directory, source), PNG)));

    const response = await prepare(fixture.app, fixture.directory, sources);

    expect(response.body.results).toEqual([
      expect.objectContaining({ source: 'valid.png', status: 'ready' }),
      { source: 'malformed.png', status: 'error' },
    ]);
  });

  it('rejects a source that the message does not reference', async () => {
    const fixture = await createFixture({ markdown: 'No image here.' });
    const response = await prepare(fixture.app, fixture.directory, fixture.sources);
    expect(response.body.results).toEqual([{ source: fixture.sources[0], status: 'error' }]);
  });

  it('does not authorize image syntax inside fenced or inline code', async () => {
    const fixture = await createFixture({
      markdown: '```md\n![fenced](FENCED)\n```\n`![inline](INLINE)`',
    });
    const sources = ['FENCED', 'INLINE'];

    const response = await prepare(fixture.app, fixture.directory, sources);

    expect(response.body.results).toEqual(sources.map((source) => ({ source, status: 'error' })));
  });

  it('rejects paths outside the workspace and approved temporary root', async () => {
    const fixture = await createFixture();
    const outsidePath = path.join(fixture.root, 'outside.png');
    await fs.writeFile(outsidePath, PNG);
    const source = new URL(`file://${outsidePath}`).toString();
    fixture.fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({
      info: { id: 'msg_1', role: 'assistant' },
      parts: [{ type: 'text', text: `![outside](${source})` }],
    }), { status: 200, headers: { 'content-type': 'application/json' } }));

    const response = await prepare(fixture.app, fixture.directory, [source]);
    expect(response.body.results).toEqual([{ source, status: 'error' }]);
  });

  it('rejects non-image bytes and symlink escapes per source', async () => {
    const fixture = await createFixture({ sources: ['invalid.png', 'linked.png'] });
    await fs.writeFile(path.join(fixture.directory, 'invalid.png'), 'not an image');
    await fs.writeFile(path.join(fixture.root, 'outside.png'), PNG);
    await fs.symlink(path.join(fixture.root, 'outside.png'), path.join(fixture.directory, 'linked.png'));

    const response = await prepare(fixture.app, fixture.directory, fixture.sources);
    expect(response.body.results).toEqual([
      { source: 'invalid.png', status: 'error' },
      { source: 'linked.png', status: 'error' },
    ]);
  });
});
