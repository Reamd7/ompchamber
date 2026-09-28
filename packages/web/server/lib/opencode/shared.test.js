/**
 * shared.js 配置读写原语的单元测试套件。
 *
 * 覆盖：parseMdFile 的 YAML frontmatter 宽容解析（EOF 结束分隔线、
 * CRLF 行尾、UTF-8 BOM、值中裸冒号、无 frontmatter），writeMdFile 的
 * 解析-改写-回写往返一致性，updateAgent 保存单字段时对无关字段的
 * 保留，以及 readConfigFile / writeConfig 的 JSONC 安全策略（注释与
 * 尾逗号可读、半截解析结果拒绝读写，防止清空用户配置——对应
 * issue #2923）。所有用例跑在按进程隔离的临时目录中。
 */
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import fs from 'fs';
import os from 'os';
import path from 'path';

import { parseMdFile, writeMdFile, readConfigFile, readConfigLayers, writeConfig } from './shared.js';
import { updateAgent } from './agents.js';
import { updateMcpConfig } from './mcp.js';

/** 用例临时目录（按进程 pid 隔离），各 describe 自行重建与清理。 */
const FIXTURE_DIR = path.join(os.tmpdir(), `ompchamber-shared-test-${process.pid}`);

/** 标准 frontmatter Markdown 样本：description / model / mode 三键加一段正文。 */
const STANDARD_MD = [
  '---',
  'description: My build agent',
  'model: anthropic/claude-sonnet-4',
  'mode: primary',
  '---',
  '',
  'This is the prompt body.',
  '',
].join('\n');

/** 把内容写入临时目录下指定相对路径（自动创建父目录）并返回绝对路径。 */
const writeFixture = (name, content) => {
  const filePath = path.join(FIXTURE_DIR, name);
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, content, 'utf8');
  return filePath;
};

/** parseMdFile：frontmatter 解析规则与边界形态。 */
describe('parseMdFile', () => {
  // 重建干净的临时目录。
  beforeEach(() => {
    fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
    fs.mkdirSync(FIXTURE_DIR, { recursive: true });
  });

  // 删除临时目录。
  afterEach(() => {
    fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
  });

  it('parses standard YAML frontmatter', () => {
    const file = writeFixture('standard.md', STANDARD_MD);
    const { frontmatter, body } = parseMdFile(file);
    expect(frontmatter).toEqual({
      description: 'My build agent',
      model: 'anthropic/claude-sonnet-4',
      mode: 'primary',
    });
    expect(body).toBe('This is the prompt body.');
  });

  it('parses frontmatter whose closing --- is at end-of-file without a trailing newline', () => {
    // gray-matter (used by OpenCode) accepts this shape; OMPChamber must too,
    // otherwise a later save duplicates the YAML block.
    const file = writeFixture('eof-close.md', [
      '---',
      'description: My build agent',
      'model: anthropic/claude-sonnet-4',
      '---',
    ].join('\n'));
    const { frontmatter, body } = parseMdFile(file);
    expect(frontmatter).toEqual({
      description: 'My build agent',
      model: 'anthropic/claude-sonnet-4',
    });
    expect(body).toBe('');
  });

  it('parses frontmatter with CRLF line endings', () => {
    const file = writeFixture('crlf.md', STANDARD_MD.replace(/\n/g, '\r\n'));
    const { frontmatter, body } = parseMdFile(file);
    expect(frontmatter.model).toBe('anthropic/claude-sonnet-4');
    expect(body).toBe('This is the prompt body.');
  });

  it('parses frontmatter preceded by a UTF-8 BOM', () => {
    const file = writeFixture('bom.md', `\uFEFF${STANDARD_MD}`);
    const { frontmatter, body } = parseMdFile(file);
    expect(frontmatter.description).toBe('My build agent');
    expect(body).toBe('This is the prompt body.');
  });

  it('falls back to lenient YAML for unquoted colons in values, matching OpenCode', () => {
    const file = writeFixture('colon.md', [
      '---',
      'description: Build agent: creates builds',
      'model: anthropic/claude-sonnet-4',
      '---',
      '',
      'Body',
      '',
    ].join('\n'));
    const { frontmatter, body } = parseMdFile(file);
    expect(frontmatter).toEqual({
      description: 'Build agent: creates builds',
      model: 'anthropic/claude-sonnet-4',
    });
    expect(body).toBe('Body');
  });

  it('treats files without frontmatter as a plain body', () => {
    const file = writeFixture('plain.md', 'Just a prompt body.');
    const { frontmatter, body } = parseMdFile(file);
    expect(frontmatter).toEqual({});
    expect(body).toBe('Just a prompt body.');
  });
});

/** writeMdFile：解析-改写-回写的往返一致性。 */
describe('writeMdFile', () => {
  // 重建干净的临时目录。
  beforeEach(() => {
    fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
    fs.mkdirSync(FIXTURE_DIR, { recursive: true });
  });

  // 删除临时目录。
  afterEach(() => {
    fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
  });

  it('round-trips a canonical single frontmatter block', () => {
    const file = writeFixture('roundtrip.md', STANDARD_MD);
    const parsed = parseMdFile(file);
    parsed.frontmatter.model = 'openai/gpt-5';
    writeMdFile(file, parsed.frontmatter, parsed.body);

    const content = fs.readFileSync(file, 'utf8');
    // Exactly one frontmatter block.
    expect(content.match(/^---\r?\n/g)).toHaveLength(1);

    const reparsed = parseMdFile(file);
    expect(reparsed.frontmatter).toEqual({
      description: 'My build agent',
      model: 'openai/gpt-5',
      mode: 'primary',
    });
    expect(reparsed.body).toBe('This is the prompt body.');
  });
});

/** updateAgent：保存单个字段时不破坏其余 frontmatter 与正文（OPE-178 回归）。 */
describe('updateAgent frontmatter preservation', () => {
  // 重建干净的临时目录。
  beforeEach(() => {
    fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
    fs.mkdirSync(FIXTURE_DIR, { recursive: true });
  });

  // 删除临时目录。
  afterEach(() => {
    fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
  });

  it('updates the model in place without duplicating YAML for a file with EOF-closed frontmatter', () => {
    // Repro of OPE-178: the file's closing --- sits at EOF (no trailing
    // newline). OpenCode parses it; OMPChamber previously treated the whole
    // file as the prompt body and prepended a second frontmatter block on save.
    const projectDir = path.join(FIXTURE_DIR, 'project');
    const agentPath = path.join(projectDir, '.opencode', 'agents', 'strateg.md');
    writeFixture(path.join('project', '.opencode', 'agents', 'strateg.md'), [
      '---',
      'description: Strategy agent',
      'model: anthropic/claude-sonnet-4',
      'temperature: 0.7',
      '---',
    ].join('\n'));

    updateAgent('strateg', { model: 'openai/gpt-5' }, projectDir);

    const content = fs.readFileSync(agentPath, 'utf8');
    expect(content.match(/^---\r?\n/g)).toHaveLength(1);

    const parsed = parseMdFile(agentPath);
    expect(parsed.frontmatter).toEqual({
      description: 'Strategy agent',
      model: 'openai/gpt-5',
      temperature: 0.7,
    });
    expect(parsed.body).toBe('');
  });

  it('preserves unrelated frontmatter fields when saving one field', () => {
    const projectDir = path.join(FIXTURE_DIR, 'project');
    const agentPath = path.join(projectDir, '.opencode', 'agents', 'strateg.md');
    writeFixture(path.join('project', '.opencode', 'agents', 'strateg.md'), [
      '---',
      'description: Strategy agent',
      'mode: primary',
      'temperature: 0.7',
      '---',
      '',
      'Body of strateg.',
      '',
    ].join('\n'));

    updateAgent('strateg', { description: 'Updated strategy agent' }, projectDir);

    const content = fs.readFileSync(agentPath, 'utf8');
    expect(content.match(/^---\r?\n/g)).toHaveLength(1);

    const parsed = parseMdFile(agentPath);
    expect(parsed.frontmatter).toEqual({
      description: 'Updated strategy agent',
      mode: 'primary',
      temperature: 0.7,
    });
    expect(parsed.body).toBe('Body of strateg.');
  });
});

/** readConfigFile / writeConfig：JSONC 解析安全与拒绝半截解析结果（issue #2923）。 */
describe('readConfigFile / writeConfig JSONC safety (issue #2923)', () => {
  // 重建干净的临时目录。
  beforeEach(() => {
    fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
    fs.mkdirSync(FIXTURE_DIR, { recursive: true });
  });

  // 删除临时目录。
  afterEach(() => {
    fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
  });

    // 合法 JSONC 样本：含注释、尾逗号与混合引号键。
  const VALID_CONFIG = [
    '{',
    '  "$schema": "https://opencode.ai/config.json",',
    '  // keep me',
    '  "plugin": ["opencode-see-image"],',
    '  "mcp": {',
    '    "openproject": {',
    '      "type": "remote",',
    '      "url": "https://openproject.example.com/mcp",',
    '      "enabled": true,',
    '    }',
    '  },',
    '  "provider": {',
    '    "ollama-cloud": {',
    '      "npm": "@ai-sdk/openai-compatible",',
    '      "name": "Ollama Cloud"',
    '    }',
    '  }',
    '}',
    '',
  ].join('\n');

  // JSON5-style unquoted keys after $schema — jsonc-parser returns a partial
  // tree of only `{ $schema }` when errors are ignored.
  // JSON5 风格样本：验证半截解析被拒绝而不是静默丢键（背景见上方英文注释）。
  const PARTIAL_PARSE_CONFIG = [
    '{',
    '  "$schema": "https://opencode.ai/config.json",',
    '  plugin: ["opencode-see-image"],',
    '  mcp: {',
    '    openproject: {',
    '      type: "remote",',
    '      url: "https://openproject.example.com/mcp",',
    '      enabled: true',
    '    }',
    '  },',
    '  provider: {',
    '    "ollama-cloud": {',
    '      npm: "@ai-sdk/openai-compatible",',
    '      name: "Ollama Cloud"',
    '    }',
    '  }',
    '}',
    '',
  ].join('\n');

  it('parses valid JSONC with comments and trailing commas without dropping keys', () => {
    const file = writeFixture('opencode.jsonc', VALID_CONFIG);
    expect(readConfigFile(file)).toEqual({
      $schema: 'https://opencode.ai/config.json',
      plugin: ['opencode-see-image'],
      mcp: {
        openproject: {
          type: 'remote',
          url: 'https://openproject.example.com/mcp',
          enabled: true,
        },
      },
      provider: {
        'ollama-cloud': {
          npm: '@ai-sdk/openai-compatible',
          name: 'Ollama Cloud',
        },
      },
    });
  });

  it('returns an empty object for a missing or whitespace-only file', () => {
    expect(readConfigFile(path.join(FIXTURE_DIR, 'missing.jsonc'))).toEqual({});
    const empty = writeFixture('empty.jsonc', '   \n');
    expect(readConfigFile(empty)).toEqual({});
  });

  it('throws INVALID_JSONC on partial-parse JSONC instead of returning a $schema-only stub', () => {
    const file = writeFixture('opencode.jsonc', PARTIAL_PARSE_CONFIG);
    expect(() => readConfigFile(file)).toThrow(/cannot be loaded safely/);
    try {
      readConfigFile(file);
    } catch (error) {
      expect(error.code).toBe('INVALID_JSONC');
    }
  });

  it('throws INVALID_JSONC for a non-object JSONC root', () => {
    const file = writeFixture('array.jsonc', '["plugin"]\n');
    expect(() => readConfigFile(file)).toThrow(/cannot be loaded safely/);
  });

  it('refuses to overwrite an unparseable config file', () => {
    const file = writeFixture('opencode.jsonc', PARTIAL_PARSE_CONFIG);
    expect(() => writeConfig({ $schema: 'https://opencode.ai/config.json' }, file)).toThrow(
      /cannot be loaded safely/,
    );
    expect(fs.readFileSync(file, 'utf8')).toBe(PARTIAL_PARSE_CONFIG);
    expect(fs.existsSync(`${file}.ompchamber.backup`)).toBe(false);
  });

  it('preserves a valid config across MCP updates', () => {
    const file = writeFixture('opencode.jsonc', VALID_CONFIG);
    const config = readConfigFile(file);
    config.mcp.openproject.enabled = false;
    writeConfig(config, file);

    const rewritten = JSON.parse(fs.readFileSync(file, 'utf8'));
    expect(rewritten.plugin).toEqual(['opencode-see-image']);
    expect(rewritten.provider['ollama-cloud'].name).toBe('Ollama Cloud');
    expect(rewritten.mcp.openproject.enabled).toBe(false);
    expect(fs.readFileSync(`${file}.ompchamber.backup`, 'utf8')).toBe(VALID_CONFIG);
  });

  it('does not wipe an unparseable user config during MCP mutation attempts', () => {
    const file = writeFixture('opencode.jsonc', PARTIAL_PARSE_CONFIG);
    const previousOpenCodeConfig = process.env.OPENCODE_CONFIG;

    try {
      process.env.OPENCODE_CONFIG = file;
      expect(() => updateMcpConfig('openproject', { enabled: true })).toThrow(
        /cannot be loaded safely/,
      );
      expect(fs.readFileSync(file, 'utf8')).toBe(PARTIAL_PARSE_CONFIG);
      expect(fs.existsSync(`${file}.ompchamber.backup`)).toBe(false);
    } finally {
      if (previousOpenCodeConfig === undefined) delete process.env.OPENCODE_CONFIG;
      else process.env.OPENCODE_CONFIG = previousOpenCodeConfig;
    }
  });

  it('returns an empty object for a comment-only config file', () => {
    const file = writeFixture('comments.jsonc', '// placeholder\n/* still empty */\n');
    expect(readConfigFile(file)).toEqual({});
  });

  it('throws INVALID_JSONC for content that yields no JSON value at all', () => {
    const yamlish = writeFixture('yamlish.jsonc', 'mcp:\n  openproject:\n    type: remote\n');
    expect(() => readConfigFile(yamlish)).toThrow(/cannot be loaded safely/);
    expect(() => writeConfig({ $schema: 'https://opencode.ai/config.json' }, yamlish)).toThrow(
      /cannot be loaded safely/,
    );
    expect(fs.readFileSync(yamlish, 'utf8')).toBe('mcp:\n  openproject:\n    type: remote\n');
    expect(fs.existsSync(`${yamlish}.ompchamber.backup`)).toBe(false);
  });

  it('keeps a valid custom layer readable when a project layer is unparseable', () => {
    const custom = writeFixture('custom.jsonc', VALID_CONFIG);
    const projectDir = path.join(FIXTURE_DIR, 'project');
    const projectFile = writeFixture(path.join('project', '.opencode', 'opencode.jsonc'), PARTIAL_PARSE_CONFIG);
    const previousOpenCodeConfig = process.env.OPENCODE_CONFIG;

    try {
      process.env.OPENCODE_CONFIG = custom;
      const layers = readConfigLayers(projectDir);
      expect(layers.customConfig.plugin).toEqual(['opencode-see-image']);
      expect(layers.projectConfig).toEqual({});
      expect(layers.mergedConfig.plugin).toEqual(['opencode-see-image']);
      expect(layers.layerErrors).toEqual([
        expect.objectContaining({
          path: projectFile,
          code: 'INVALID_JSONC',
        }),
      ]);

      updateMcpConfig('openproject', { enabled: false }, projectDir);
      const rewritten = JSON.parse(fs.readFileSync(custom, 'utf8'));
      expect(rewritten.plugin).toEqual(['opencode-see-image']);
      expect(rewritten.mcp.openproject.enabled).toBe(false);
      expect(fs.readFileSync(projectFile, 'utf8')).toBe(PARTIAL_PARSE_CONFIG);
      expect(fs.existsSync(`${projectFile}.ompchamber.backup`)).toBe(false);
    } finally {
      if (previousOpenCodeConfig === undefined) delete process.env.OPENCODE_CONFIG;
      else process.env.OPENCODE_CONFIG = previousOpenCodeConfig;
    }
  });
});
