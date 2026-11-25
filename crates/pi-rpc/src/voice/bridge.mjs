// Embedded by pi-rpc; never run the CLI or load a user's Pi extensions.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { closeSync, fstatSync, openSync, readSync, realpathSync } from 'node:fs';
import * as module from 'node:module';
import { isAbsolute, join } from 'node:path';
import { pathToFileURL } from 'node:url';

const files = {
  'lib/voice-session.ts': 'bbe4516414969c95c1f18d01e327a717f57adef77703df08ca83f7717cf23435',
  'lib/voice-install.ts': '0eadf7ae9cf7957b074bf0e067df225b6405666e45991af04570e86e63fcac2d',
  'lib/voice-artifacts.ts': '2346d55c40c9ba5dd3d79bb17ce8f5d552d2935e41dc1943b69ba03b85f3286c',
  'lib/voice-platform.ts': 'b055a69be124083c63fb7c1e4a2f1b2aa006698c664d453670b2943dee2c9f94',
  'lib/agent-home.ts': '6d9628d42ea7547884844ded76d9a15f77f913844fc444a13ed957cbec021f28',
  'lib/gentle-shell-bundled.ts': '539049b26b924068dde5cf1a80f1777c79807ab615195b6735a871d29a6d3761',
  'scripts/voice-worker.cjs': 'd73026804fce647c8bcfcb0772c23c35bd21416f71c1f8b9b36c13ff40dba09a',
};

const workerFile = 'scripts/voice-worker.cjs';
// Leave room for Windows quoting in the 32767 UTF-16 unit command line.
function boundedFile(path, limit) {
  const fd = openSync(path, 'r');
  try {
    if (!fstatSync(fd).isFile()) throw new Error();
    const bytes = Buffer.alloc(limit + 1);
    let count = 0;
    while (count < bytes.length) {
      const n = readSync(fd, bytes, count, bytes.length - count);
      if (!n) return bytes.subarray(0, count);
      count += n;
    }
    throw new Error();
  } finally { closeSync(fd); }
}

export function supportsManifest(manifest, mode) {
  return manifest.name === 'gentle-pi' && (manifest.version === '4.9.0' ||
    (mode === 'bundled' && manifest.version === '5.0.0'));
}

async function load(root, mode, setup) {
  if (!isAbsolute(root) || root !== realpathSync(root)) throw new Error();
  const manifest = JSON.parse(boundedFile(join(root, 'package.json'), 64 * 1024).toString('utf8'));
  if (!supportsManifest(manifest, mode)) throw new Error();
  const sources = new Map();
  let worker;
  for (const [file, digest] of Object.entries(files)) {
    const path = join(root, file);
    if (realpathSync(path) !== path) throw new Error();
    const bytes = boundedFile(path, file === workerFile ? 22 * 1024 : 128 * 1024);
    if (createHash('sha256').update(bytes).digest('hex') !== digest) throw new Error();
    if (file === workerFile) worker = bytes.toString('utf8');
    else sources.set(pathToFileURL(path).href, bytes.toString('utf8'));
  }
  const hook = module.registerHooks({
    resolve(specifier, context, next) {
      if (sources.has(specifier)) return { url: specifier, shortCircuit: true };
      if (sources.has(context.parentURL) && !specifier.startsWith('node:')) {
        const url = new URL(specifier, context.parentURL).href;
        if (!sources.has(url)) throw new Error();
        return { url, shortCircuit: true };
      }
      return next(specifier, context);
    },
    load(url, context, next) {
      if (!sources.has(url)) return next(url, context);
      return { format: 'module', shortCircuit: true, source: module.stripTypeScriptTypes(sources.get(url)) };
    },
  });
  try {
    const get = file => import(pathToFileURL(join(root, 'lib', file)).href);
    const entries = ['voice-session.ts', 'voice-platform.ts', 'voice-install.ts', 'gentle-shell-bundled.ts'];
    if (setup) entries.push('voice-artifacts.ts');
    return [worker, ...await Promise.all(entries.map(get))];
  } finally {
    hook.deregister();
  }
}

const emit = (type, fields) => process.stdout.write(JSON.stringify({ type, ...fields }) + '\n');
const codes = new Set(('unsupported node-version not-installed invalid-request runtime-invalid model-invalid runtime-load model-load no-input-device unsupported-input capture empty-audio no-speech decode internal timeout protocol worker-exited cancelled').split(' '));

async function voiceContext(root, mode, home, setup = false) {
  let api;
  try { api = await load(root, mode, setup); } catch { return; }
  const [, , platform, install, bundled] = api;
  const diagnosis = platform.diagnoseVoicePlatform(platform.readVoicePlatformProbe());
  if (diagnosis.support !== 'known' || !isAbsolute(home)) return;
  const { target } = diagnosis;
  const env = mode === 'bundled' && bundled.runningBundledPrefix({ GENTLE_SHELL_PREFIX: home })
    ? { GENTLE_SHELL_PREFIX: home }
    : mode === 'managed' ? { GENTLE_PI_CONFIG_HOME: home } : undefined;
  if (!env) return;
  return { api, target, voiceHome: install.resolveVoiceHome(env) };
}

export async function main(root, mode, home) {
  const context = await voiceContext(root, mode, home);
  if (!context) return emit('error', { code: 'unsupported' });
  const { api: [worker, session, , install], target, voiceHome } = context;
  await serve(onStateChange => session.createVoiceSession({
    target, voiceHome, onStateChange,
    spawn: (_executable, _args, options) => spawnVerifiedWorker(worker, options),
  }), async () => {
    const { model, runtime } = await install.readVoiceStatus({ target, voiceHome });
    return model.state === 'installed' && runtime?.state === 'installed';
  });
}

// The pinned worker uses only builtins and absolute runtime require paths.
// In Node's eval-mode CommonJS module, make its normal entry guard run.
export function spawnVerifiedWorker(source, options) {
  return spawn(process.execPath, ['--input-type=commonjs', '-e', `"use strict"; require.main = module;\n${source}`], options);
}

// One session per pipe. Cancel (including stdin EOF) takes precedence over
// any asynchronous result until a terminal record is emitted.
export function serve(create, ready = async () => false) {
  return new Promise(resolve => {
    let phase = 'idle', session, line = '';
    const finish = (type, fields, exitCode = 0) => {
      phase = 'done';
      process.stdin.pause();
      process.stdin.removeListener('data', data);
      process.stdin.removeListener('end', eof);
      emit(type, fields);
      process.exitCode = exitCode;
      resolve();
    };
    const fail = error => {
      if (phase !== 'closing' && phase !== 'done') finish('error', { code: codes.has(error?.code) ? error.code : 'internal' });
    };
    const cancel = async protocol => {
      if (phase === 'closing' || phase === 'done') return;
      phase = 'closing';
      try { await session?.cancel(); } catch { return finish('error', { code: 'internal' }); }
      if (protocol) finish('error', { code: 'protocol' }, 1);
      else finish('cancelled');
    };
    const stop = async () => {
      phase = 'stopping';
      try {
        const { text, truncated } = await session.stop();
        if (phase !== 'stopping') return;
        if (typeof text !== 'string' || text.length > 16384 || typeof truncated !== 'boolean') {
          finish('error', { code: 'protocol' }, 1);
        } else finish('result', { text, truncated });
      } catch (error) { fail(error); }
    };
    const state = state => {
      if (state === 'recording' && phase === 'starting') {
        phase = 'recording';
        emit('recording');
      } else if (phase === 'recording' || phase === 'stopping') {
        if (state === 'transcribing') emit('transcribing');
        if (state === 'finished' && phase === 'recording') void stop();
      }
    };
    const command = async cmd => {
      if (cmd === 'cancel') return cancel(false);
      if (cmd === 'probe' && phase === 'idle') {
        phase = 'probing';
        const installed = await ready();
        if (phase === 'probing') {
          if (installed) finish('ready');
          else finish('error', { code: 'not-installed' });
        }
      } else if (cmd === 'start' && phase === 'idle') {
        phase = 'starting';
        session = create(state);
        await session.start();
      } else if (cmd === 'stop' && phase === 'recording') await stop();
      else if (phase !== 'closing' && phase !== 'done') await cancel(true);
    };
    const data = chunk => {
      for (const byte of chunk) {
        if (phase === 'done' || phase === 'closing') return;
        if (byte === 10) {
          const cmd = line;
          line = '';
          void command(cmd).catch(fail);
        } else if (line.length === 32 || byte < 32 || byte > 126) {
          void cancel(true);
          return;
        } else line += String.fromCharCode(byte);
      }
    };
    const eof = () => void cancel(false);
    process.stdin.on('data', data).once('end', eof).once('error', eof);
  });
}
