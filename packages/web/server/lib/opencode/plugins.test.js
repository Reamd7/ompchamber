/**
 * plugins.js OpenCode 插件数据层的单元测试（bun:test）。
 *
 * 通过 OPENCODE_CONFIG 指向临时用户配置文件并动态导入被测模块，覆盖：
 * 插件条目的解析与序列化（字符串与 [spec, options] 元组）、非法 spec 与
 * 文件名的拒绝、创建/更新/删除及其在 user/project 层的路由、OPENCODE_CONFIG
 * 变更后的重新解析、损坏项目层的容错列表、条目 id 编解码，以及插件目录文件
 * 的写入/读取/删除/覆盖与合法性过滤。
 */
import { afterAll, beforeAll, beforeEach, describe, expect, test } from 'bun:test';
import fs from 'fs';
import os from 'os';
import path from 'path';

// beforeAll 创建的临时根目录（afterAll 删除）。
let rootDir;
// beforeEach 重建的临时项目目录。
let projectDir;
// 临时用户级配置文件路径（经 OPENCODE_CONFIG 注入被测模块）。
let userConfigPath;
// beforeAll 中动态导入的被测模块（等环境变量就绪后再加载）。
let plugins;

/** 执行 fn 并返回其抛出的 Error；未抛出时反而抛出「Expected function to throw」。 */
function thrownBy(fn) {
  try {
    fn();
  } catch (error) {
    return error;
  }
  throw new Error('Expected function to throw');
}

/** 将 data 以两空格缩进的 JSON 写入 filePath（自动创建父目录）。 */
function writeJson(filePath, data) {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, JSON.stringify(data, null, 2), 'utf8');
}

/** 读取并解析 filePath 的 JSON 内容。 */
function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf8'));
}

// 套件：插件条目与插件目录文件在 user/project 配置层上的解析与增删改查。
describe('opencode plugins data layer', () => {
  // 建立临时根目录、把 OPENCODE_CONFIG 指向临时用户配置，并动态导入被测模块。
  beforeAll(async () => {
    rootDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-plugins-'));
    userConfigPath = path.join(rootDir, 'user-opencode.json');
    process.env.OPENCODE_CONFIG = userConfigPath;
    plugins = await import('./plugins.js');
  });

  // 每个用例重置用户级配置并新建独立的项目目录。
  beforeEach(() => {
    process.env.OPENCODE_CONFIG = userConfigPath;
    projectDir = fs.mkdtempSync(path.join(rootDir, 'project-'));
    fs.rmSync(userConfigPath, { force: true });
  });

  // 删除临时根目录并清除 OPENCODE_CONFIG 环境变量。
  afterAll(() => {
    fs.rmSync(rootDir, { recursive: true, force: true });
    delete process.env.OPENCODE_CONFIG;
  });

  test('parses raw plugin entries', () => {
    expect(plugins.parsePluginRaw('foo')).toEqual({ spec: 'foo' });
    expect(plugins.parsePluginRaw('foo@1.0.0')).toEqual({ spec: 'foo@1.0.0' });
    expect(plugins.parsePluginRaw(['foo', { a: 1 }])).toEqual({ spec: 'foo', options: { a: 1 } });
    expect(plugins.parsePluginRaw(['foo', {}])).toEqual({ spec: 'foo', options: {} });
    expect(() => plugins.parsePluginRaw(123)).toThrow('Plugin spec');
  });

  test('serializes plugin entries', () => {
    expect(plugins.serializePluginEntry({ spec: 'foo' })).toBe('foo');
    expect(plugins.serializePluginEntry({ spec: 'foo', options: undefined })).toBe('foo');
    expect(plugins.serializePluginEntry({ spec: 'foo', options: {} })).toBe('foo');
    expect(plugins.serializePluginEntry({ spec: 'foo', options: { a: 1 } })).toEqual(['foo', { a: 1 }]);
  });

  test('rejects invalid specs and file names', () => {
    expect(() => plugins.createPluginEntry({ spec: 123, scope: 'user' }, projectDir)).toThrow('Plugin spec');
    expect(() => plugins.writePluginDirFile({ fileName: '', content: '', scope: 'project' }, projectDir)).toThrow('Plugin file name');
    expect(() => plugins.writePluginDirFile({ fileName: '../bad.js', content: '', scope: 'project' }, projectDir)).toThrow('Plugin file name');
    expect(() => plugins.writePluginDirFile({ fileName: 'a/b.js', content: '', scope: 'project' }, projectDir)).toThrow('Plugin file name');
    expect(() => plugins.writePluginDirFile({ fileName: 'A.js', content: '', scope: 'project' }, projectDir)).toThrow('Plugin file name');
    expect(() => plugins.writePluginDirFile({ fileName: 'foo.txt', content: '', scope: 'project' }, projectDir)).toThrow('Plugin file name');
  });

  test('creates string and tuple entries with duplicate rejection', () => {
    plugins.createPluginEntry({ spec: 'plain-plugin', scope: 'user' }, projectDir);
    plugins.createPluginEntry({ spec: 'tuple-plugin', options: { apiKey: 'x' }, scope: 'user' }, projectDir);

    expect(readJson(userConfigPath).plugin).toEqual(['plain-plugin', ['tuple-plugin', { apiKey: 'x' }]]);
    expect(() => plugins.createPluginEntry({ spec: 'plain-plugin', scope: 'user' }, projectDir)).toThrow('already exists');
    expect(thrownBy(() => plugins.createPluginEntry({ spec: 'plain-plugin', scope: 'user' }, projectDir))).toHaveProperty('code', 'ENTRY_EXISTS');
  });

  test('routes project entries to project config and user entries to custom user config', () => {
    plugins.createPluginEntry({ spec: 'user-plugin', scope: 'user' }, projectDir);
    plugins.createPluginEntry({ spec: 'project-plugin', scope: 'project' }, projectDir);

    expect(readJson(userConfigPath).plugin).toEqual(['user-plugin']);
    expect(readJson(path.join(projectDir, '.opencode', 'opencode.json')).plugin).toEqual(['project-plugin']);
  });

  test('re-resolves custom config env between calls', () => {
    const firstConfigPath = path.join(rootDir, 'first', 'opencode.json');
    const secondConfigPath = path.join(rootDir, 'second', 'opencode.json');

    process.env.OPENCODE_CONFIG = firstConfigPath;
    plugins.createPluginEntry({ spec: 'first-plugin', scope: 'user' }, projectDir);
    plugins.writePluginDirFile({ fileName: 'first.js', content: 'one', scope: 'user' }, projectDir);

    process.env.OPENCODE_CONFIG = secondConfigPath;
    plugins.createPluginEntry({ spec: 'second-plugin', scope: 'user' }, projectDir);
    plugins.writePluginDirFile({ fileName: 'second.js', content: 'two', scope: 'user' }, projectDir);

    expect(readJson(firstConfigPath).plugin).toEqual(['first-plugin']);
    expect(readJson(secondConfigPath).plugin).toEqual(['second-plugin']);
    expect(fs.existsSync(path.join(path.dirname(firstConfigPath), 'plugins', 'first.js'))).toBe(true);
    expect(fs.existsSync(path.join(path.dirname(secondConfigPath), 'plugins', 'second.js'))).toBe(true);
  });

  test('updates entries in place and transitions between string and tuple', () => {
    writeJson(userConfigPath, { plugin: ['first', ['second', { a: 1 }], 'third'] });

    plugins.updatePluginEntry(plugins.encodePluginId('config', 'user:second'), { spec: 'second-new', options: {} }, projectDir);
    expect(readJson(userConfigPath).plugin).toEqual(['first', 'second-new', 'third']);

    plugins.updatePluginEntry(plugins.encodePluginId('config', 'user:first'), { spec: 'first-new', options: { b: 2 } }, projectDir);
    expect(readJson(userConfigPath).plugin).toEqual([['first-new', { b: 2 }], 'second-new', 'third']);
  });

  test('deletes entries and prunes empty plugin key', () => {
    writeJson(userConfigPath, { plugin: ['only'] });

    plugins.deletePluginEntry(plugins.encodePluginId('config', 'user:only'), projectDir);
    expect(readJson(userConfigPath)).toEqual({});
  });

  test('lists user plugins when a project layer is unparseable', () => {
    const partialProject = [
      '{',
      '  "$schema": "https://opencode.ai/config.json",',
      '  plugin: ["broken-project-plugin"],',
      '}',
      '',
    ].join('\n');
    writeJson(userConfigPath, { plugin: ['user-plugin'] });
    const projectFile = path.join(projectDir, '.opencode', 'opencode.jsonc');
    fs.mkdirSync(path.dirname(projectFile), { recursive: true });
    fs.writeFileSync(projectFile, partialProject, 'utf8');

    expect(plugins.listPluginEntries(projectDir).map((entry) => entry.spec)).toEqual(['user-plugin']);
    expect(fs.readFileSync(projectFile, 'utf8')).toBe(partialProject);
    expect(fs.existsSync(`${projectFile}.ompchamber.backup`)).toBe(false);
  });

  test('lists entries from user and project layers with scopes and parsed kinds', () => {
    writeJson(userConfigPath, { plugin: ['npm-plugin', '/abs/plugin.js', '@scope/pkg@1.0.0'] });
    writeJson(path.join(projectDir, '.opencode', 'opencode.json'), { plugin: ['./local-plugin.js'] });

    const entries = plugins.listPluginEntries(projectDir);
    expect(entries).toEqual([
      expect.objectContaining({ spec: 'npm-plugin', scope: 'user', kind: 'config', parsedKind: 'npm', sourcePath: userConfigPath }),
      expect.objectContaining({ spec: '/abs/plugin.js', scope: 'user', kind: 'config', parsedKind: 'path', sourcePath: userConfigPath }),
      expect.objectContaining({ spec: '@scope/pkg@1.0.0', scope: 'user', kind: 'config', parsedKind: 'npm', sourcePath: userConfigPath }),
      expect.objectContaining({ spec: './local-plugin.js', scope: 'project', kind: 'config', parsedKind: 'path', sourcePath: path.join(projectDir, '.opencode', 'opencode.json') }),
    ]);
    fs.rmSync(userConfigPath, { force: true });
    expect(plugins.listPluginEntries(projectDir)).toEqual([
      expect.objectContaining({ spec: './local-plugin.js', scope: 'project' }),
    ]);
  });

  test('encodes and decodes ids', () => {
    const id = plugins.encodePluginId('config', 'user:oh-my-openagent@4.3.0');
    expect(plugins.decodePluginId(id)).toEqual({ prefix: 'config', value: 'user:oh-my-openagent@4.3.0' });
  });

  test('round-trips plugin dir files', () => {
    plugins.writePluginDirFile({ fileName: 'my-plugin.ts', content: 'export default {}', scope: 'project' }, projectDir);
    const file = plugins.listPluginDirFiles(projectDir).find((candidate) => candidate.fileName === 'my-plugin.ts');

    expect(file).toEqual(expect.objectContaining({ fileName: 'my-plugin.ts', scope: 'project', kind: 'file' }));
    expect(plugins.readPluginDirFile(file.id, projectDir)).toEqual({ fileName: 'my-plugin.ts', scope: 'project', content: 'export default {}' });
    plugins.deletePluginDirFile(file.id, projectDir);
    expect(plugins.listPluginDirFiles(projectDir).filter((candidate) => candidate.scope === 'project')).toEqual([]);
    expect(() => plugins.deletePluginDirFile(file.id, projectDir)).toThrow('not found');
  });

  test('rejects duplicate plugin dir files unless overwrite is true', () => {
    plugins.writePluginDirFile({ fileName: 'dup.js', content: 'one', scope: 'project' }, projectDir);
    expect(() => plugins.writePluginDirFile({ fileName: 'dup.js', content: 'two', scope: 'project' }, projectDir)).toThrow('already exists');
    expect(thrownBy(() => plugins.writePluginDirFile({ fileName: 'dup.js', content: 'two', scope: 'project' }, projectDir))).toHaveProperty('code', 'FILE_EXISTS');

    plugins.writePluginDirFile({ fileName: 'dup.js', content: 'two', scope: 'project' }, projectDir, { overwrite: true });
    expect(fs.readFileSync(path.join(projectDir, '.opencode', 'plugins', 'dup.js'), 'utf8')).toBe('two');
  });

  test('lists only valid plugin dir files', () => {
    const dir = path.join(projectDir, '.opencode', 'plugins');
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, 'valid.mjs'), '', 'utf8');
    fs.writeFileSync(path.join(dir, 'README.md'), '', 'utf8');

    expect(plugins.listPluginDirFiles(projectDir).filter((file) => file.scope === 'project')).toEqual([
      expect.objectContaining({ fileName: 'valid.mjs', scope: 'project' }),
    ]);
  });
});
