// Setup never invokes the capture session or worker. Only this embedded entry
// point can request publication, and only of the pinned installer's payload.
import { lstat, mkdir, mkdtemp, opendir, realpath, rm } from 'node:fs/promises';
import { basename, join as setupJoin, resolve } from 'node:path';

const emitSetup = (type, fields) => process.stdout.write(JSON.stringify({ type, ...fields }) + '\n');
const installCodes = new Set(['timeout', 'http', 'redirect', 'size', 'integrity', 'archive', 'unsupported', 'io', 'unsafe-path', 'invalid-component', 'not-ready', 'backend-changed']);
const setupError = code => Object.assign(new Error(), { code });
const componentParent = kind => kind === 'runtime' ? 'runtime' : 'models';

// Linux only: an existing path must be a real directory owned by root or
// this uid, without group/other write access. A root-owned sticky ancestor
// (e.g. /tmp) protects an existing trusted child, but not an absent child.
async function checkedDirectory(path, uid, allowMissing = false) {
  if (!path.startsWith('/') || resolve(path) !== path) throw setupError('unsafe-path');
  let current = '/', sticky = false;
  for (const part of ['', ...path.slice(1).split('/').filter(Boolean)]) {
    current = setupJoin(current, part);
    let info;
    try { info = await lstat(current); } catch (error) {
      if (error.code === 'ENOENT' && allowMissing && !sticky) return false;
      throw setupError('unsafe-path');
    }
    if (!info.isDirectory() || (info.uid !== 0 && info.uid !== uid) || await realpath(current) !== current) {
      throw setupError('unsafe-path');
    }
    sticky = (info.mode & 0o022) !== 0;
    if (sticky && (current === path || info.uid !== 0 || (info.mode & 0o1000) === 0)) throw setupError('unsafe-path');
  }
  return true;
}

async function checkComponent(path, uid, signal) {
  const root = await lstat(path);
  const directories = [[path, 0]];
  let count = 0;
  while (directories.length) {
    const [directory, depth] = directories.pop();
    if (depth > 32) throw setupError('invalid-component');
    for await (const entry of await opendir(directory)) {
      signal.throwIfAborted();
      if (++count > 16_384) throw setupError('invalid-component');
      const file = setupJoin(directory, entry.name);
      const info = await lstat(file);
      if ((info.uid !== 0 && info.uid !== uid) || (info.mode & 0o022) !== 0 || info.dev !== root.dev) throw setupError('invalid-component');
      if (info.isDirectory()) directories.push([file, depth + 1]);
      else if (!info.isFile() || info.nlink !== 1) throw setupError('invalid-component');
    }
  }
  // Authoritative status reads this file synchronously with respect to its
  // parse, so bound its size and reject non-regular entries before calling it.
  const marker = await lstat(setupJoin(path, '.gentle-voice.json')).catch(() => undefined);
  if (!marker?.isFile() || marker.size > 1024 * 1024) throw setupError('invalid-component');
}

export async function installMissing({ install, artifacts, target, voiceHome, anchor }, { signal, progress, admit, publish }) {
  const uid = process.getuid?.();
  if (process.platform !== 'linux' || uid === undefined) throw setupError('unsupported');
  if (voiceHome !== setupJoin(anchor, 'voice') && voiceHome !== setupJoin(anchor, 'config/gentle-ai/voice')) throw setupError('unsafe-path');
  await checkedDirectory(anchor, uid);
  const components = [
    ['runtime', artifacts.voiceRuntimeManifest(target), options => install.installVoiceRuntime({ ...options, target })],
    ['model', artifacts.PARAKEET_MODEL_MANIFEST, options => install.installVoiceModel(options)],
  ];
  async function status(kind, manifest, home = voiceHome) {
    signal.throwIfAborted();
    const path = install.voiceComponentPath(home, kind, manifest);
    if (!/^[a-z0-9][a-z0-9._-]{0,127}$/.test(manifest.id) || path !== setupJoin(home, componentParent(kind), manifest.id)) throw setupError('unsafe-path');
    const exists = await checkedDirectory(path, uid, true);
    if (exists) await checkComponent(path, uid, signal);
    const { state } = await install.readVoiceComponentStatus({ voiceHome: home, kind, manifest });
    if (state !== (exists ? 'installed' : 'missing')) throw setupError('invalid-component');
    return state;
  }
  for (const [kind, manifest] of components) await status(kind, manifest);
  let workspace;
  try {
    for (const [kind, manifest, acquire] of components) {
      if (await status(kind, manifest) === 'installed') continue;
      signal.throwIfAborted();
      await mkdir(voiceHome, { recursive: true, mode: 0o700 });
      await checkedDirectory(voiceHome, uid);
      workspace ??= await mkdtemp(setupJoin(voiceHome, '.desktop-setup-'));
      await admit(kind);
      signal.throwIfAborted();
      await acquire({ voiceHome: workspace, signal, onProgress(event) {
        // Upstream publish refers to the workspace, not the live install.
        if (event.component === kind && (event.phase === 'download' || event.phase === 'extract')) {
          progress({ component: kind, phase: event.phase, received: event.overallReceivedBytes, total: event.overallTotalBytes });
        }
      } });
      if (await status(kind, manifest) === 'installed') continue;
      if (await status(kind, manifest, workspace) !== 'installed') throw setupError('not-ready');
      signal.throwIfAborted();
      const parent = setupJoin(voiceHome, componentParent(kind));
      await mkdir(parent, { recursive: true, mode: 0o700 });
      await checkedDirectory(parent, uid);
      signal.throwIfAborted();
      progress({ component: kind, phase: 'publish', received: 0, total: 0 });
      await publish({ workspace: basename(workspace), component: kind, id: manifest.id });
      if (await status(kind, manifest) !== 'installed') throw setupError('not-ready');
    }
    for (const [kind, manifest] of components) {
      if (await status(kind, manifest) !== 'installed') throw setupError('not-ready');
    }
  } finally {
    // Never remove or move any live component, even after a racing cancel.
    if (workspace) await rm(workspace, { recursive: true, force: true });
  }
}

export function serveSetup(run) {
  return new Promise(resolveDone => {
    const controller = new AbortController();
    const { signal } = controller;
    let started = false, done = false, protocol = false, pending, line = '';
    let lastKey, lastTime = -Infinity;
    const finish = (type, fields) => {
      done = true;
      process.stdin.removeListener('data', data);
      process.stdin.removeListener('end', eof);
      // Setup owns this input for the lifetime of its helper. Close it on a
      // terminal record even when the caller has not closed its write end.
      process.stdin.destroy();
      emitSetup(type, fields);
      process.exitCode = protocol ? 1 : 0;
      resolveDone();
    };
    const settle = error => {
      if (protocol) finish('error', { code: 'protocol' });
      else if (signal.aborted) finish('cancelled');
      else if (error) finish('error', { code: installCodes.has(error.code) ? error.code : 'io' });
      else finish('ready');
    };
    const cancel = bad => {
      protocol ||= bad;
      controller.abort();
      pending?.reject(signal.reason);
      pending = undefined;
      if (!started) settle();
    };
    const progress = ({ component, phase, received, total }) => {
      if (signal.aborted || !['runtime', 'model'].includes(component) || !['download', 'extract', 'publish'].includes(phase) ||
          !Number.isSafeInteger(received) || !Number.isSafeInteger(total) || received < 0 || received > total) return;
      const key = `${component}:${phase}`, now = performance.now();
      if (lastKey === key && now - lastTime < 250) return;
      lastKey = key;
      lastTime = now;
      emitSetup('progress', { component, phase, received, total });
    };
    const request = (type, fields, replies, timeout) => {
      let timer;
      return new Promise((resolve, reject) => {
        signal.throwIfAborted();
        if (pending) throw setupError('protocol');
        pending = { resolve, reject, replies };
        if (timeout) timer = setTimeout(() => {
          pending = undefined;
          reject(setupError('timeout'));
        }, timeout);
        emitSetup(type, fields);
      }).finally(() => clearTimeout(timer));
    };
    const admit = component => request('admit', { component }, new Map([
      [`admitted-${component}`, null], ['admission-denied', 'backend-changed'],
    ]), 10_000);
    const publish = fields => request('publish', fields, new Map([
      ['published', null], ['publish-conflict', null], ['publish-error', 'io'],
      ['unsafe-path', 'unsafe-path'], ['backend-changed', 'backend-changed'],
    ]));
    const command = cmd => {
      if (cmd === 'cancel') cancel(false);
      else if (cmd === 'install' && !started) {
        started = true;
        Promise.resolve().then(() => { signal.throwIfAborted(); return run({ signal, progress, admit, publish }); }).then(() => settle(), settle);
      } else if (pending?.replies.has(cmd)) {
        const { resolve, reject, replies } = pending;
        pending = undefined;
        const code = replies.get(cmd);
        if (code) reject(setupError(code));
        else resolve(); // A publish conflict is acceptable only if status now says installed.
      } else cancel(true);
    };
    const data = chunk => {
      for (const byte of chunk) {
        if (done || signal.aborted) return;
        if (byte === 10) {
          const cmd = line;
          line = '';
          command(cmd);
        } else if (line.length === 32 || byte < 32 || byte > 126) cancel(true);
        else line += String.fromCharCode(byte);
      }
    };
    const eof = () => { if (!done) cancel(false); };
    process.stdin.on('data', data).once('end', eof).once('error', eof);
  });
}

async function setupMain(root, mode, home) {
  const context = await voiceContext(root, mode, home, true);
  if (!context || mode !== 'bundled' || process.platform !== 'linux' || context.voiceHome !== setupJoin(home, 'config/gentle-ai/voice')) {
    return emitSetup('error', { code: 'unsupported' });
  }
  const { api: [, , , install, , artifacts], target, voiceHome } = context;
  await serveSetup(options => installMissing({ install, artifacts, target, voiceHome, anchor: home }, options));
}
