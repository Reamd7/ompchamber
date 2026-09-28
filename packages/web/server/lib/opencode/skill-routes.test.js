/**
 * skill-routes.js 的 HTTP 层测试套件：在随机端口启动真实 express 应用并注入
 * 桩依赖（目录解析固定指向临时项目、伪造的 omp-host engine /skill 端点），
 * 端到端验证技能列表的目录软回退、engine 技能行的合并保留与 scope 推断。
 */
import { afterEach, describe, expect, it } from 'vitest';
import express from 'express';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { registerSkillRoutes } from './skill-routes.js';
import {
  createSkill,
  deleteSkill,
  discoverSkills,
  getSkillSources,
  isManagedSkillPath,
  mergeDiscoveredSkills,
  renameSkill,
  updateSkill,
} from './skills.js';
import {
  SKILL_DIR,
  SKILL_SCOPE,
  deleteSkillSupportingFile,
  readSkillSupportingFile,
  writeSkillSupportingFile,
} from './shared.js';

/** 创建带 .git 标记的临时项目目录（被目录解析桩视为合法 project root），供用例布置技能文件。 */
const createTempProject = () => {
  const projectRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'oc-skill-routes-'));
  fs.mkdirSync(path.join(projectRoot, '.git'));
  return projectRoot;
};

/**
 * 启动挂载了技能路由的测试 Express 应用；engineSkills 提供时会额外启动一个
 * 伪造的 omp-host engine face（GET /skill 返回 engine 形态的 wire 行），
 * 使路由内部的代理拉取走真实 HTTP。其余依赖均为固定桩：目录解析指向
 * projectRoot，设置读取返回空对象，catalog 扫描安装一律返回失败/空。
 * @param {object} options projectRoot 必填；engineSkills 可选的 engine 行数组；homeDir 可选的 homedir 覆盖
 * @returns {{ baseUrl: string, close: () => Promise<void> }} 应用基址与关闭回调（连同伪造 engine 一并关闭）
 */
const startSkillsApp = ({ projectRoot, engineSkills = null, homeDir = null }) => {
  const app = express();
  app.use(express.json());

  // Fake omp-host engine face: serves GET /skill with engine-shaped wire rows
  // ({name, description, location, content}) exactly as the host emits them.
  let engineServer = null;
  let enginePort = 0;
  if (engineSkills) {
    const engine = express();
    engine.get('/skill', (_req, res) => res.json(engineSkills));
    engineServer = engine.listen(0);
    enginePort = engineServer.address().port;
  }

  registerSkillRoutes(app, {
    fs,
    path,
    os: homeDir ? { homedir: () => homeDir } : os,
    resolveProjectDirectory: async () => ({ directory: projectRoot, error: null }),
    resolveOptionalProjectDirectory: async (req) => {
      const queryDirectory = Array.isArray(req.query?.directory)
        ? req.query.directory[0]
        : req.query?.directory;
      if (!queryDirectory) {
        return { directory: null, error: null };
      }
      return { directory: String(queryDirectory), error: null };
    },
    readSettingsFromDisk: async () => ({}),
    sanitizeSkillCatalogs: (value) => value,
    isUnsafeSkillRelativePath: () => false,
    refreshOpenCodeAfterConfigChange: async () => {},
    clientReloadDelayMs: 0,
    buildOpenCodeUrl: () => `http://127.0.0.1:${enginePort}/`,
    getOpenCodeAuthHeaders: () => ({}),
    getOpenCodePort: () => enginePort,
    getSkillSources,
    discoverSkills,
    mergeDiscoveredSkills,
    createSkill,
    updateSkill,
    deleteSkill,
    renameSkill,
    isManagedSkillPath,
    readSkillSupportingFile,
    writeSkillSupportingFile,
    deleteSkillSupportingFile,
    SKILL_SCOPE,
    SKILL_DIR,
    getCuratedSkillsSources: () => [],
    getCacheKey: () => 'k',
    scanWithCache: async (_key, loader) => loader(),
    parseSkillRepoSource: () => ({ ok: false }),
    scanSkillsRepository: async () => ({ ok: false }),
    installSkillsFromRepository: async () => ({ ok: false }),
    fetchGitHubRepoMetas: async () => ({}),
    getProfiles: () => [],
    getProfile: () => null,
  });

  const server = app.listen(0);
  const { port } = server.address();
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    close: () => new Promise((resolve, reject) => {
      server.close((appError) => {
        if (!engineServer) {
          return appError ? reject(appError) : resolve();
        }
        engineServer.close((engineError) => {
          if (appError || engineError) {
            reject(appError || engineError);
          } else {
            resolve();
          }
        });
      });
    }),
  };
};

/** 验证 engine 缺席或遗漏条目时，技能列表回退到本地目录发现并正确标记可重命名性。 */
describe('skill-routes directory soft fallback', () => {
  /** @type {string | null} */
  let projectRoot = null;
  /** @type {{ close: () => Promise<void> } | null} */
  let appHandle = null;

  afterEach(async () => {
    if (appHandle) {
      await appHandle.close();
      appHandle = null;
    }
    if (projectRoot) {
      fs.rmSync(projectRoot, { recursive: true, force: true });
      projectRoot = null;
    }
  });

  it('lists repository-local .agents skills after create even when list omits directory', async () => {
    projectRoot = createTempProject();
    appHandle = startSkillsApp({ projectRoot });

    const createResponse = await fetch(`${appHandle.baseUrl}/api/config/skills/repo-local-skill`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        description: 'Created without list directory',
        instructions: 'Do the thing.',
        scope: 'project',
        source: 'agents',
      }),
    });
    expect(createResponse.status).toBe(200);
    expect(fs.existsSync(path.join(projectRoot, '.agents', 'skills', 'repo-local-skill', 'SKILL.md'))).toBe(true);

    const listResponse = await fetch(`${appHandle.baseUrl}/api/config/skills`);
    expect(listResponse.status).toBe(200);
    const payload = await listResponse.json();
    expect(payload.skills.map((skill) => skill.name)).toContain('repo-local-skill');
    const skill = payload.skills.find((entry) => entry.name === 'repo-local-skill');
    expect(skill.scope).toBe('project');
    expect(skill.source).toBe('agents');
  });

  it('lists manually created repository-local .agents skills via active-project fallback', async () => {
    projectRoot = createTempProject();
    const skillDir = path.join(projectRoot, '.agents', 'skills', 'manual-repo-skill');
    fs.mkdirSync(skillDir, { recursive: true });
    fs.writeFileSync(
      path.join(skillDir, 'SKILL.md'),
      [
        '---',
        'name: manual-repo-skill',
        'description: Manual repository skill',
        '---',
        '',
        'Instructions',
        '',
      ].join('\n'),
      'utf8',
    );

    appHandle = startSkillsApp({ projectRoot });
    const listResponse = await fetch(`${appHandle.baseUrl}/api/config/skills`);
    expect(listResponse.status).toBe(200);
    const payload = await listResponse.json();
    expect(payload.skills.map((skill) => skill.name)).toContain('manual-repo-skill');
  });

  it('marks managed-root skills renamable and cache skills not renamable', async () => {
    projectRoot = createTempProject();
    const managedDir = path.join(projectRoot, '.opencode', 'skills', 'managed-list-skill');
    fs.mkdirSync(managedDir, { recursive: true });
    fs.writeFileSync(
      path.join(managedDir, 'SKILL.md'),
      [
        '---',
        'name: managed-list-skill',
        'description: Managed list skill',
        '---',
        '',
        'Managed body',
        '',
      ].join('\n'),
      'utf8',
    );

    const cacheStamp = `oc-skill-routes-${Date.now()}`;
    const cacheDir = path.join(os.homedir(), '.cache', 'opencode', 'skills', cacheStamp, 'cache-list-skill');
    fs.mkdirSync(cacheDir, { recursive: true });
    fs.writeFileSync(
      path.join(cacheDir, 'SKILL.md'),
      [
        '---',
        'name: cache-list-skill',
        'description: Cache list skill',
        '---',
        '',
        'Cache body',
        '',
      ].join('\n'),
      'utf8',
    );

    try {
      appHandle = startSkillsApp({ projectRoot });
      const listResponse = await fetch(
        `${appHandle.baseUrl}/api/config/skills?directory=${encodeURIComponent(projectRoot)}`,
      );
      expect(listResponse.status).toBe(200);
      const payload = await listResponse.json();

      const managed = payload.skills.find((entry) => entry.name === 'managed-list-skill');
      const cached = payload.skills.find((entry) => entry.name === 'cache-list-skill');

      expect(managed).toBeTruthy();
      expect(managed.renamable).toBe(true);
      expect(cached).toBeTruthy();
      expect(cached.renamable).toBe(false);
    } finally {
      fs.rmSync(path.join(os.homedir(), '.cache', 'opencode', 'skills', cacheStamp), {
        recursive: true,
        force: true,
      });
    }
  });
});

/** 验证 engine 返回的 wire 行在合并后原样保留，以及 .omp/skills 位置到 project/user scope 的推断。 */
describe('skill-routes engine skills merge and scope inference', () => {
  /** @type {string | null} */
  let projectRoot = null;
  /** @type {{ close: () => Promise<void> } | null} */
  let appHandle = null;

  afterEach(async () => {
    if (appHandle) {
      await appHandle.close();
      appHandle = null;
    }
    if (projectRoot) {
      fs.rmSync(projectRoot, { recursive: true, force: true });
      projectRoot = null;
    }
  });

  it('keeps engine-shaped rows through the fetchOpenCodeDiscoveredSkills merge', async () => {
    projectRoot = createTempProject();
    const engineSkillPath = path.join(projectRoot, '.agents', 'skills', 'engine-row', 'SKILL.md');

    appHandle = startSkillsApp({
      projectRoot,
      engineSkills: [
        {
          name: 'engine-row',
          description: 'Discovered by the engine',
          location: engineSkillPath,
          content: 'Engine body without frontmatter',
        },
      ],
    });

    const listResponse = await fetch(
      `${appHandle.baseUrl}/api/config/skills?directory=${encodeURIComponent(projectRoot)}`,
    );
    expect(listResponse.status).toBe(200);
    const payload = await listResponse.json();

    const merged = payload.skills.find((entry) => entry.name === 'engine-row');
    expect(merged).toBeTruthy();
    expect(merged.path).toBe(engineSkillPath);
    expect(merged.description).toBe('Discovered by the engine');
    expect(merged.content).toBe('Engine body without frontmatter');
    expect(merged.scope).toBe(SKILL_SCOPE.PROJECT);
  });

  it('classifies .omp/skills engine locations to project and user scopes', async () => {
    projectRoot = createTempProject();
    const fakeHome = fs.mkdtempSync(path.join(os.tmpdir(), 'oc-skill-home-'));
    const projectSkillPath = path.join(projectRoot, '.omp', 'skills', 'deploy-helper', 'SKILL.md');
    const userSkillPath = path.join(fakeHome, '.omp', 'agent', 'skills', 'global-helper', 'SKILL.md');

    try {
      appHandle = startSkillsApp({
        projectRoot,
        homeDir: fakeHome,
        engineSkills: [
          {
            name: 'deploy-helper',
            description: 'Project omp skill',
            location: projectSkillPath,
            content: 'Project omp body',
          },
          {
            name: 'global-helper',
            description: 'User omp skill',
            location: userSkillPath,
            content: 'User omp body',
          },
        ],
      });

      const listResponse = await fetch(
        `${appHandle.baseUrl}/api/config/skills?directory=${encodeURIComponent(projectRoot)}`,
      );
      expect(listResponse.status).toBe(200);
      const payload = await listResponse.json();

      const projectSkill = payload.skills.find((entry) => entry.name === 'deploy-helper');
      const userSkill = payload.skills.find((entry) => entry.name === 'global-helper');
      expect(projectSkill).toBeTruthy();
      expect(projectSkill.scope).toBe(SKILL_SCOPE.PROJECT);
      expect(userSkill).toBeTruthy();
      expect(userSkill.scope).toBe(SKILL_SCOPE.USER);
    } finally {
      fs.rmSync(fakeHome, { recursive: true, force: true });
    }
  });
});
