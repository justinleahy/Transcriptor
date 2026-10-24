import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { releaseMetadata, stageAssets, prepareReleaseAssets, publishRelease } from './release.mjs';

const version = '0.1.0';
const metadata = {version, tag: `v${version}`, prerelease: false};
const packageNames = ['macos-arm64.dmg', 'windows-x64-cpu-setup.exe', 'windows-x64-cuda-setup.exe',
  'windows-x64-cuda-portable.zip', 'linux-x64.AppImage'].map(suffix => `Transcriptor-${version}-${suffix}`);
function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'transcriptor-release-'));
  t.after(() => rmSync(root, {recursive: true, force: true}));
  return root;
}
function assets(t) {
  const directory = fixture(t);
  for (const name of packageNames) writeFileSync(join(directory, name), `bytes of ${name}`);
  return directory;
}
function versions(root, tauri = version, packageVersion = version) {
  mkdirSync(join(root, 'src-tauri'), {recursive: true});
  writeFileSync(join(root, 'package.json'), JSON.stringify({version: packageVersion}));
  writeFileSync(join(root, 'src-tauri/tauri.conf.json'), JSON.stringify({version: tauri}));
  writeFileSync(join(root, 'src-tauri/Cargo.toml'), `[package]\nname = "transcriptor"\nversion = "${packageVersion}"\n`);
}

test('validates matching package versions and tag before release builds', t => {
  const root = fixture(t); versions(root);
  assert.deepEqual(releaseMetadata(root, 'refs/tags/v0.1.0'), metadata);
  assert.deepEqual(releaseMetadata(root, 'refs/heads/main'), metadata);
  assert.throws(() => releaseMetadata(root, 'refs/tags/v9.0.0'), /does not match/);
  versions(root, '0.2.0');
  assert.throws(() => releaseMetadata(root), /must match/);
});

test('recognizes prerelease tags and rejects unsafe version strings', t => {
  const root = fixture(t), beta = '0.2.0-beta.1'; versions(root, beta, beta);
  assert.deepEqual(releaseMetadata(root, `refs/tags/v${beta}`), {version: beta, tag: `v${beta}`, prerelease: true});
  versions(root, '../invalid', '../invalid');
  assert.throws(() => releaseMetadata(root), /must be X.Y.Z/);
});

test('stages distinguishable CPU/CUDA installers and portable ZIP', t => {
  const root = fixture(t), bundle = join(root, 'bundle');
  for (const dir of ['nsis', 'cuda-portable']) mkdirSync(join(bundle, dir), {recursive: true});
  writeFileSync(join(bundle, 'nsis/setup.exe'), 'installer');
  writeFileSync(join(bundle, 'cuda-portable/package.zip'), 'portable');
  const common = {bundle, version, platform: 'windows', arch: 'x64'};
  const cpu = stageAssets({...common, flavor: 'cpu', output: join(root, 'cpu')});
  const cuda = stageAssets({...common, flavor: 'cuda', output: join(root, 'cuda')});
  assert.deepEqual(cpu, [packageNames[1]]);
  assert.deepEqual(cuda, [packageNames[2], packageNames[3]]);
});

test('preserves recognized macOS/Linux file extensions', t => {
  const root = fixture(t);
  for (const [platform, arch, directory, file, expected] of [
    ['macos', 'arm64', 'dmg', 'app.dmg', packageNames[0]],
    ['linux', 'x64', 'appimage', 'app.AppImage', packageNames[4]],
  ]) {
    const bundle = join(root, platform); mkdirSync(join(bundle, directory), {recursive: true});
    writeFileSync(join(bundle, directory, file), 'package');
    assert.deepEqual(stageAssets({bundle, version, platform, arch, flavor: 'cpu', output: join(root, platform + '-assets')}), [expected]);
  }
});

test('fails on missing or ambiguous build outputs', t => {
  const root = fixture(t), bundle = join(root, 'bundle'); mkdirSync(join(bundle, 'nsis'), {recursive: true});
  const config = {bundle, version, platform: 'windows', arch: 'x64', flavor: 'cpu', output: join(root, 'out')};
  assert.throws(() => stageAssets(config), /exactly one/);
  for (const name of ['a.exe', 'b.exe']) writeFileSync(join(bundle, 'nsis', name), 'installer');
  assert.throws(() => stageAssets(config), /exactly one/);
});

test('requires every package and creates correct checksums', async t => {
  const directory = assets(t);
  const names = await prepareReleaseAssets(directory, version);
  assert.equal(names.length, 6);
  for (const line of readFileSync(join(directory, 'SHA256SUMS.txt'), 'utf8').trim().split('\n')) {
    const [hash, name] = line.split('  ');
    assert.equal(hash, createHash('sha256').update(readFileSync(join(directory, name))).digest('hex'));
  }
  rmSync(join(directory, packageNames[2]));
  await assert.rejects(prepareReleaseAssets(directory, version), /Missing release asset/);
});

test('rejects unexpected files and empty packages', async t => {
  const directory = assets(t); writeFileSync(join(directory, 'extra.pfx'), 'not a release asset');
  await assert.rejects(prepareReleaseAssets(directory, version), /exactly the five/);
  rmSync(join(directory, 'extra.pfx')); writeFileSync(join(directory, packageNames[0]), '');
  await assert.rejects(prepareReleaseAssets(directory, version), /Empty/);
});

function github(t, {existing = null, failUpload = false, oldAssets = [], corruptUpload = false} = {}) {
  const directory = assets(t), calls = [];
  const draft = {id: 42, draft: true, body: '<!-- transcriptor-release-pipeline -->', upload_url: 'https://uploads.example/releases/42/assets{?name,label}'};
  const request = async (url, options) => {
    calls.push({url, method: options.method, body: typeof options.body === 'string' ? JSON.parse(options.body) : null});
    if (url.includes('uploads.example')) {
      let size = 0;
      for await (const chunk of options.body) size += chunk.length;
      return {ok: !failUpload, status: failUpload ? 500 : 201,
        json: async () => ({state: 'uploaded', size: corruptUpload ? size + 1 : size})};
    }
    if (url.includes('/releases/tags/')) return {ok: !!existing, status: existing ? 200 : 404, json: async () => existing};
    if (options.method === 'POST') return {ok: true, status: 201, json: async () => draft};
    if (url.includes('/assets?')) return {ok: true, status: 200, json: async () => oldAssets};
    if (options.method === 'DELETE') return {ok: true, status: 204};
    if (options.method === 'PATCH') return {ok: true, status: 200, json: async () => ({html_url: 'https://github.example/release'})};
    throw new Error(`Unexpected request ${url}`);
  };
  return {calls, options: {directory, metadata, repository: 'owner/transcriptor', commit: 'a'.repeat(40), token: 'fake-test-token', request}};
}

test('publishes only after all six assets upload to a draft', async t => {
  const {calls, options} = github(t);
  assert.equal(await publishRelease(options), 'https://github.example/release');
  assert.equal(calls.find(call => call.method === 'POST').body.draft, true);
  assert.equal(calls.filter(call => call.url.includes('uploads.example')).length, 6);
  assert.deepEqual(calls.at(-1).body, {draft: false});
});

test('a rerun replaces only assets in a pipeline-owned draft', async t => {
  const existing = {id: 42, draft: true, body: '<!-- transcriptor-release-pipeline -->', upload_url: 'https://uploads.example/releases/42/assets{?name,label}'};
  const {calls, options} = github(t, {existing, oldAssets: [{id: 7, name: packageNames[0]}]});
  await publishRelease(options);
  assert.equal(calls.filter(call => call.method === 'DELETE').length, 1);
  assert.ok(calls.findIndex(call => call.method === 'DELETE') < calls.findIndex(call => call.url.includes('uploads.example')));
  assert.deepEqual(calls.at(-1).body, {draft: false});
});

test('leaves a draft unpublished if GitHub reports an incomplete upload', async t => {
  const {calls, options} = github(t, {corruptUpload: true});
  await assert.rejects(publishRelease(options), /Upload verification failed/);
  assert.equal(calls.some(call => call.method === 'PATCH'), false);
});

test('does not remove an unrelated attachment from a draft', async t => {
  const existing = {id: 42, draft: true, body: '<!-- transcriptor-release-pipeline -->'};
  const {calls, options} = github(t, {existing, oldAssets: [{id: 9, name: 'manual.txt'}]});
  await assert.rejects(publishRelease(options), /not owned/);
  assert.equal(calls.some(call => call.method === 'DELETE'), false);
});

test('upload failure leaves release in draft state', async t => {
  const {calls, options} = github(t, {failUpload: true});
  await assert.rejects(publishRelease(options), /remains a draft/);
  assert.equal(calls.some(call => call.method === 'PATCH'), false);
});

test('will not overwrite a published or independently managed release', async t => {
  for (const existing of [{draft: false}, {draft: true, body: 'Manual draft'}]) {
    const {calls, options} = github(t, {existing});
    await assert.rejects(publishRelease(options), /Refusing to overwrite/);
    assert.equal(calls.length, 1);
  }
});

test('streams real file bytes through HTTP with the correct content lengths', async t => {
  const directory = assets(t), uploads = [];
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    response.setHeader('Content-Type', 'application/json');
    if (request.url.includes('/releases/tags/')) {
      response.writeHead(404); response.end('{}');
    } else if (request.url.startsWith('/upload')) {
      uploads.push({name: new URL(request.url, api).searchParams.get('name'), body,
        length: request.headers['content-length'], authorization: request.headers.authorization});
      response.writeHead(201); response.end(JSON.stringify({state: 'uploaded', size: body.length}));
    } else if (request.method === 'POST') {
      response.writeHead(201); response.end(JSON.stringify({id: 42, upload_url: `${api}/upload{?name,label}`}));
    } else if (request.method === 'PATCH') {
      response.end(JSON.stringify({html_url: 'https://github.example/release'}));
    } else { response.writeHead(500); response.end('{}'); }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const api = `http://127.0.0.1:${server.address().port}`;
  await publishRelease({directory, metadata, repository: 'owner/transcriptor', commit: 'a'.repeat(40),
    token: 'fake-test-token', api});
  assert.equal(uploads.length, 6);
  for (const upload of uploads) {
    assert.deepEqual(upload.body, readFileSync(join(directory, upload.name)));
    assert.equal(Number(upload.length), upload.body.length);
    assert.equal(upload.authorization, 'Bearer fake-test-token');
  }
});
