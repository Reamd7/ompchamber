/**
 * Downloads and extracts local sherpa-onnx STT model archives.
 * Archives (.tar.bz2) come from the k2-fsa GitHub releases and are extracted
 * with the system `tar` into the speech-models directory.
 */
/**
 * 本地语音模型（STT/TTS）的按需下载与安装。
 *
 * 归档为 k2-fsa GitHub releases 的 .tar.bz2，用系统 tar 解压到语音
 * 模型目录；下载先写临时文件再改名，解压经 staging 目录校验后才
 * 落位，保证中断或失败绝不留下会被误判为"已安装"的半成品文件。
 */

import { createWriteStream } from 'fs';
import { mkdir, rename, rm, stat } from 'fs/promises';
import path from 'path';
import { Readable } from 'stream';
import { pipeline } from 'stream/promises';
import { spawn } from 'child_process';

import { getLocalSttModelSpec } from './model-catalog.js';

/**
 * 检查模型目录是否具备全部必需文件：目录条目（如 espeak-ng-data）
 * 视为满足，文件必须存在且非空；任一缺失即返回 false。
 */
async function hasRequiredFiles(modelDir, requiredFiles) {
  const results = await Promise.all(
    requiredFiles.map(async (rel) => {
      try {
        const s = await stat(path.join(modelDir, rel));
        if (s.isDirectory()) {
          return true;
        }
        return s.isFile() && s.size > 0;
      } catch {
        return false;
      }
    }),
  );
  return results.every(Boolean);
}

/**
 * 流式下载 url 到 outputPath：先写 .tmp 临时文件、成功后原子改名；
 * onProgress(downloadedBytes, totalBytes) 汇报进度（总大小未知时为
 * null）；任何失败都会清理临时文件后抛出。
 */
async function downloadToFile(url, outputPath, onProgress) {
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`Failed to download ${url}: ${res.status} ${res.statusText}`);
  }
  if (!res.body) {
    throw new Error(`Failed to download ${url}: missing response body`);
  }

  const totalBytes = Number.parseInt(res.headers.get('content-length') || '', 10) || null;
  let downloadedBytes = 0;

  const tmpPath = `${outputPath}.tmp-${Date.now()}`;
  await mkdir(path.dirname(outputPath), { recursive: true });

  const nodeStream = Readable.fromWeb(res.body);
  if (typeof onProgress === 'function') {
    nodeStream.on('data', (chunk) => {
      downloadedBytes += chunk.length;
      onProgress(downloadedBytes, totalBytes);
    });
  }

  try {
    await pipeline(nodeStream, createWriteStream(tmpPath));
    await rename(tmpPath, outputPath);
  } catch (error) {
    await rm(tmpPath, { force: true }).catch(() => undefined);
    throw error;
  }
}

/** 用系统 tar 解压归档到目标目录；tar 启动失败或退出码非 0 时拒绝。 */
async function extractTarArchive(archivePath, destDir) {
  await mkdir(destDir, { recursive: true });

  await new Promise((resolve, reject) => {
    const child = spawn('tar', ['xf', archivePath, '-C', destDir], {
      stdio: 'ignore',
      windowsHide: true,
    });
    child.on('error', reject);
    child.on('exit', (code) => {
      if (code === 0) {
        resolve();
      } else {
        reject(new Error(`tar exited with code ${code}`));
      }
    });
  });
}

/** 判断文件存在且非空（用于判断缓存的归档是否可信）。 */
async function isNonEmptyFile(filePath) {
  try {
    const s = await stat(filePath);
    return s.isFile() && s.size > 0;
  } catch {
    return false;
  }
}

/**
 * Check whether a model is fully installed (all required files present).
 * @param {string} modelsDir
 * @param {string} modelId
 * @returns {Promise<boolean>}
 */
/**
 * 判断模型是否已完整安装（全部必需文件就位）。
 */
export async function isLocalSttModelInstalled(modelsDir, modelId) {
  const spec = getLocalSttModelSpec(modelId);
  return hasRequiredFiles(path.join(modelsDir, spec.extractedDir), spec.requiredFiles);
}

/**
 * Ensure a model is downloaded and extracted. Resolves with the model dir.
 *
 * Extraction is staged: the archive unpacks into a temporary directory and is
 * verified before being renamed into place. An interrupted or failed tar must
 * never leave partial files at the final path — the installed check only
 * verifies file presence, so a truncated .onnx there would be treated as an
 * installed model forever ("Protobuf parsing failed" at load time).
 *
 * @param {{ modelsDir: string, modelId: string,
 *           onProgress?: (downloadedBytes: number, totalBytes: number | null) => void }} options
 * @returns {Promise<string>}
 */
/**
 * 确保模型已下载并解压，完成时返回模型目录。
 *
 * 已安装则直接返回；存在残缺目录（上次中断的产物）时先删除再重试；
 * 归档缺失时下载，解压进 staging 目录并校验必需文件后才改名落位；
 * 校验不过或解压失败都会丢弃缓存的归档，让下次重试重新下载，
 * 避免损坏的归档被永久当作已安装模型。
 */
export async function ensureLocalSttModel({ modelsDir, modelId, onProgress }) {
  const spec = getLocalSttModelSpec(modelId);
  const modelDir = path.join(modelsDir, spec.extractedDir);
  if (await hasRequiredFiles(modelDir, spec.requiredFiles)) {
    return modelDir;
  }

  // A directory that exists but fails the required-files check is a partial
  // extraction from an earlier interrupted attempt — remove it before retrying.
  await rm(modelDir, { recursive: true, force: true }).catch(() => undefined);

  const downloadsDir = path.join(modelsDir, '.downloads');
  const archiveFilename = path.basename(new URL(spec.archiveUrl).pathname);
  const archivePath = path.join(downloadsDir, archiveFilename);

  if (!(await isNonEmptyFile(archivePath))) {
    await downloadToFile(spec.archiveUrl, archivePath, onProgress);
  }

  const stagingDir = path.join(modelsDir, `.staging-${spec.extractedDir}-${Date.now()}`);
  try {
    await extractTarArchive(archivePath, stagingDir);

    const stagedModelDir = path.join(stagingDir, spec.extractedDir);
    if (!(await hasRequiredFiles(stagedModelDir, spec.requiredFiles))) {
      // Bad archive (truncated download / corrupt cache): drop it so the next
      // attempt re-downloads instead of re-extracting the same broken bytes.
      await rm(archivePath, { force: true }).catch(() => undefined);
      throw new Error(
        `Extracted ${archiveFilename}, but required model files are missing or empty. The archive was discarded; retry to re-download.`,
      );
    }

    await rename(stagedModelDir, modelDir);
  } catch (error) {
    await rm(stagingDir, { recursive: true, force: true }).catch(() => undefined);
    // Any extraction failure means the cached archive can't be trusted
    // (corrupt bz2, truncated download). Discard it so retry re-downloads.
    await rm(archivePath, { force: true }).catch(() => undefined);
    throw error;
  }
  await rm(stagingDir, { recursive: true, force: true }).catch(() => undefined);

  await rm(archivePath, { force: true }).catch(() => undefined);

  return modelDir;
}
