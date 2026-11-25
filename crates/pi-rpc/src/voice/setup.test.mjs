import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { chmod, lstat, mkdir, mkdtemp, readFile, readdir, realpath, rename, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { installMissing } from './setup.mjs';

const linux = { skip: process.platform !== 'linux' };

test('the embedded setup entry parses as one module', async () => {
  const bridge = await readFile(new URL('./bridge.mjs', import.meta.url), 'utf8');
  const setup = await readFile(new URL('./setup.mjs', import.meta.url), 'utf8');
  const result = spawnSync(process.execPath, ['--input-type=module', '--check'], {
    input: `${bridge}\n${setup}\nawait setupMain(...process.argv.slice(1));\n`,
    encoding: 'utf8',
  });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
});

async function check(input, expected, code = 0, { setup, replies = {} } = {}) {
  const child = spawn(process.execPath, ['--input-type=module', '-e', `
    import { serveSetup } from './crates/pi-rpc/src/voice/setup.mjs';
    await serveSetup(${setup ?? `async ({ signal, progress }) => {
      if (signal.aborted) return;
      progress({ component: 'runtime', phase: 'download', received: 1, total: 2 });
      progress({ component: 'runtime', phase: 'download', received: 2, total: 2 });
      progress({ component: 'runtime', phase: 'publish', received: 2, total: 2 });
    }`});`], { stdio: 'pipe', env: process.env.SystemRoot ? { SystemRoot: process.env.SystemRoot } : {} });
  const closed = new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', resolve);
  });
  let stdout = '', stderr = '';
  child.stdout.setEncoding('utf8').on('data', chunk => {
    stdout += chunk;
    for (const [record, reply] of Object.entries(replies)) {
      if (stdout.includes(record + '\n')) {
        delete replies[record];
        child.stdin.write(reply);
      }
    }
  });
  child.stderr.setEncoding('utf8').on('data', chunk => { stderr += chunk; });
  if (input === null) child.stdin.end();
  else child.stdin.write(input);
  assert.equal(await closed, code);
  assert.equal(stderr, '');
  assert.equal(stdout, expected.map(line => JSON.stringify(line) + '\n').join(''));
}

test('setup coalesces same-phase progress without losing phase transitions or terminal', { timeout: 5000 }, () => check('install\n', [
  { type: 'progress', component: 'runtime', phase: 'download', received: 1, total: 2 },
  { type: 'progress', component: 'runtime', phase: 'publish', received: 2, total: 2 },
  { type: 'ready' },
]));
test('capture verbs are not setup commands', { timeout: 5000 }, () => check('start\n', [{ type: 'error', code: 'protocol' }], 1));
test('EOF before install cancels without progress', { timeout: 5000 }, () => check(null, [{ type: 'cancelled' }]));
test('a buffered cancel prevents installation', { timeout: 5000 }, () => check('install\ncancel\n', [{ type: 'cancelled' }]));

const admission = `async ({ admit, progress, signal }) => {
  for (const component of ['runtime', 'model']) {
    await admit(component);
    signal.throwIfAborted();
    progress({ component, phase: 'download', received: 0, total: 1 });
  }
}`;
const runtimeAdmission = { type: 'admit', component: 'runtime' };
const modelAdmission = { type: 'admit', component: 'model' };
const runtimeDownload = { type: 'progress', component: 'runtime', phase: 'download', received: 0, total: 1 };
const admissionOptions = (modelReply) => ({
  setup: admission,
  replies: {
    [JSON.stringify(runtimeAdmission)]: 'admitted-runtime\n',
    [JSON.stringify(modelAdmission)]: modelReply,
  },
});

test('each matching admission unlocks just its own download', { timeout: 5000 }, () => check('install\n', [
  runtimeAdmission, runtimeDownload, modelAdmission, { ...runtimeDownload, component: 'model' }, { type: 'ready' },
], 0, admissionOptions('admitted-model\n')));
test('denied model admission never starts the second download', { timeout: 5000 }, () => check('install\n', [
  runtimeAdmission, runtimeDownload, modelAdmission, { type: 'error', code: 'backend-changed' },
], 0, admissionOptions('admission-denied\n')));
test('replayed runtime admission does not authorize the model download', { timeout: 5000 }, () => check('install\n', [
  runtimeAdmission, runtimeDownload, modelAdmission, { type: 'error', code: 'protocol' },
], 1, admissionOptions('admitted-runtime\n')));
test('cancel while awaiting model admission does not start the download', { timeout: 5000 }, () => check('install\n', [
  runtimeAdmission, runtimeDownload, modelAdmission, { type: 'cancelled' },
], 0, admissionOptions('cancel\nadmitted-model\n')));

async function fixture(t) {
  const anchor = await mkdtemp(join(await realpath(tmpdir()), 'pi-voice-setup-'));
  t.after(() => rm(anchor, { recursive: true, force: true }));
  const voiceHome = join(anchor, 'config/gentle-ai/voice');
  const path = (home, kind, manifest) => join(home, kind === 'runtime' ? 'runtime' : 'models', manifest.id);
  const runtime = { id: 'test-runtime' }, model = { id: 'test-model' };
  const seen = [];
  const controller = new AbortController();
  const read = async ({ voiceHome, kind, manifest }) => {
    const dest = path(voiceHome, kind, manifest);
    const present = await lstat(dest).catch(error => { if (error.code !== 'ENOENT') throw error; });
    if (!present) return { state: 'missing' };
    const marker = await readFile(join(dest, '.gentle-voice.json'), 'utf8').catch(() => undefined);
    return { state: marker === 'verified' ? 'installed' : 'invalid' };
  };
  async function seed(home, kind, manifest, marker = 'verified') {
    const dest = path(home, kind, manifest);
    await mkdir(dest, { recursive: true, mode: 0o700 });
    await writeFile(join(dest, '.gentle-voice.json'), marker, { mode: 0o600 });
  }
  const context = {
    anchor, voiceHome, target: 'test',
    artifacts: { voiceRuntimeManifest: () => runtime, PARAKEET_MODEL_MANIFEST: model },
    install: {
      voiceComponentPath: path, readVoiceComponentStatus: read,
      async installVoiceRuntime({ voiceHome, signal }) {
        signal.throwIfAborted();
        seen.push('acquire runtime');
        await seed(voiceHome, 'runtime', runtime);
      },
      async installVoiceModel({ voiceHome, signal }) {
        signal.throwIfAborted();
        seen.push('acquire model');
        await seed(voiceHome, 'model', model);
      },
    },
  };
  const options = {
    signal: controller.signal, progress() {}, async admit() {},
    async publish({ workspace, component, id }) {
      seen.push(`publish ${component}`);
      await rename(path(join(voiceHome, workspace), component, { id }), path(voiceHome, component, { id }));
    },
  };
  return { anchor, voiceHome, runtime, model, path, seen, seed, context, options, controller };
}

test('acquires only missing assets and does not rewrite a ready runtime', linux, async t => {
  const f = await fixture(t);
  await f.seed(f.voiceHome, 'runtime', f.runtime);
  const runtime = f.path(f.voiceHome, 'runtime', f.runtime);
  const inode = (await lstat(runtime)).ino;
  await installMissing(f.context, f.options);
  assert.deepEqual(f.seen, ['acquire model', 'publish model']);
  assert.equal((await lstat(runtime)).ino, inode);
  assert.deepEqual((await readdir(f.voiceHome)).sort(), ['models', 'runtime']);
});

test('invalid model prevents even runtime acquisition and stays unchanged', linux, async t => {
  const f = await fixture(t);
  await f.seed(f.voiceHome, 'model', f.model, 'damaged');
  await assert.rejects(installMissing(f.context, f.options), { code: 'invalid-component' });
  assert.deepEqual(f.seen, []);
  assert.deepEqual(await readdir(f.voiceHome), ['models']);
  assert.equal(await readFile(join(f.path(f.voiceHome, 'model', f.model), '.gentle-voice.json'), 'utf8'), 'damaged');
});

test('rejects an aliased or writable ancestor before creating files', linux, async t => {
  const f = await fixture(t);
  const config = join(f.anchor, 'config');
  const outside = join(f.anchor, 'outside');
  await mkdir(outside, { mode: 0o700 });
  await symlink(outside, config);
  await assert.rejects(installMissing(f.context, f.options), { code: 'unsafe-path' });
  assert.deepEqual(await readdir(outside), []);
  await rm(config);
  await mkdir(config, { mode: 0o700 });
  await chmod(config, 0o777);
  await assert.rejects(installMissing(f.context, f.options), { code: 'unsafe-path' });
  assert.deepEqual(await readdir(config), []);
  assert.deepEqual(f.seen, []);
});

test('cancel after runtime publication preserves it and skips model', linux, async t => {
  const f = await fixture(t);
  await assert.rejects(installMissing(f.context, {
    ...f.options,
    async publish(request) { await f.options.publish(request); f.controller.abort(); },
  }), { name: 'AbortError' });
  assert.deepEqual(f.seen, ['acquire runtime', 'publish runtime']);
  assert.deepEqual(await readdir(f.voiceHome), ['runtime']);
  assert.equal(await readFile(join(f.path(f.voiceHome, 'runtime', f.runtime), '.gentle-voice.json'), 'utf8'), 'verified');
});

test('drift after runtime publication preserves runtime and never calls the model installer', linux, async t => {
  const f = await fixture(t);
  const requests = [];
  await assert.rejects(installMissing(f.context, {
    ...f.options,
    async admit(component) {
      requests.push(component);
      if (component === 'model') throw Object.assign(new Error(), { code: 'backend-changed' });
    },
  }), { code: 'backend-changed' });
  assert.deepEqual(requests, ['runtime', 'model']);
  assert.deepEqual(f.seen, ['acquire runtime', 'publish runtime']);
  assert.deepEqual(await readdir(f.voiceHome), ['runtime']);
  assert.equal(await readFile(join(f.path(f.voiceHome, 'runtime', f.runtime), '.gentle-voice.json'), 'utf8'), 'verified');
});

for (const [name, alter, code] of [
  ['aliased directory', async (staged, f) => {
    await rename(staged, join(f.anchor, 'aliased-runtime'));
    await symlink(join(f.anchor, 'aliased-runtime'), staged);
  }, 'unsafe-path'],
  ['writable file', async staged => {
    await chmod(join(staged, '.gentle-voice.json'), 0o666);
  }, 'invalid-component'],
]) {
  test(`rejects a staged ${name} without calling the publisher`, linux, async t => {
    const f = await fixture(t);
    const install = f.context.install;
    const acquire = install.installVoiceRuntime;
    install.installVoiceRuntime = async options => {
      await acquire(options);
      await alter(f.path(options.voiceHome, 'runtime', f.runtime), f);
    };
    await assert.rejects(installMissing(f.context, f.options), { code });
    assert.deepEqual(f.seen, ['acquire runtime']);
    assert.deepEqual(await readdir(f.voiceHome), []);
    if (name === 'aliased directory') {
      assert.equal(await readFile(join(f.anchor, 'aliased-runtime/.gentle-voice.json'), 'utf8'), 'verified');
    }
  });
}

test('a successful transport acknowledgement cannot fake authoritative readiness', linux, async t => {
  const f = await fixture(t);
  await assert.rejects(installMissing(f.context, { ...f.options, async publish() {} }), { code: 'not-ready' });
  assert.deepEqual(f.seen, ['acquire runtime']);
  assert.deepEqual(await readdir(dirname(f.path(f.voiceHome, 'runtime', f.runtime))), []);
});
