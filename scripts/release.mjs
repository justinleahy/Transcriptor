import { createHash } from 'node:crypto';
import { createReadStream, copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const MARKER = '<!-- transcriptor-release-pipeline -->';

export function releaseMatrix(cudaOnly = false) {
  const include = [
    {os: 'macos-latest', flavor: 'cpu'},
    {os: 'windows-latest', flavor: 'cpu'},
    {os: 'ubuntu-latest', flavor: 'cpu'},
    {os: 'windows-2022', flavor: 'cuda'},
  ];
  return {include: cudaOnly ? include.filter(job => job.flavor === 'cuda') : include};
}

export function cleanBuildOutputs(root) {
  // Preserve compiled dependencies and the pinned toolkit on a persistent runner.
  for (const path of ['release-assets', 'src-tauri/target/release/bundle']) {
    rmSync(join(root, path), {recursive: true, force: true});
  }
}

export function releaseMetadata(root, ref = '') {
  const pkg = JSON.parse(readFileSync(join(root, 'package.json')));
  const tauri = JSON.parse(readFileSync(join(root, 'src-tauri/tauri.conf.json')));
  const cargo = readFileSync(join(root, 'src-tauri/Cargo.toml'), 'utf8').split(/^\[/m)[1];
  const cargoVersion = cargo?.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?$/.test(pkg.version)) {
    throw new Error('Release version must be X.Y.Z or X.Y.Z-prerelease');
  }
  if (pkg.version !== tauri.version || pkg.version !== cargoVersion) {
    throw new Error('Versions in package.json, tauri.conf.json and Cargo.toml must match');
  }
  const tag = `v${pkg.version}`;
  if (ref.startsWith('refs/tags/') && ref !== `refs/tags/${tag}`) {
    throw new Error(`Tag ${ref} does not match application version ${tag}`);
  }
  return {version: pkg.version, tag, prerelease: pkg.version.includes('-')};
}

export function previewMetadata(metadata, {eventName, ref = '', runNumber, signed = true, cudaOnly = true}) {
  if (eventName !== 'workflow_dispatch' || !ref.startsWith('refs/heads/') || signed || cudaOnly) {
    throw new Error('An unsigned preview requires a manual full-platform branch build');
  }
  if (!/^[1-9]\d*$/.test(String(runNumber ?? ''))) throw new Error('Preview requires a valid workflow run number');
  return {...metadata, tag: `preview-${metadata.version}-${runNumber}`, prerelease: true, unsigned: true};
}

export function stageAssets({bundle, output, version, platform, arch, flavor}) {
  if (!['x64', 'arm64'].includes(arch)) throw new Error(`Unsupported architecture: ${arch}`);
  const definitions = {
    'macos:cpu': [['dmg', '.dmg', '.dmg']],
    'windows:cpu': [['nsis', '.exe', '-cpu-setup.exe']],
    'windows:cuda': [['nsis', '.exe', '-cuda-setup.exe'], ['cuda-portable', '.zip', '-cuda-portable.zip']],
    'linux:cpu': [['appimage', '.AppImage', '.AppImage']],
  };
  const selected = definitions[`${platform}:${flavor}`];
  if (!selected) throw new Error(`Unsupported release variant: ${platform}/${flavor}`);
  if (existsSync(output) && readdirSync(output).length) throw new Error('Asset staging directory must be empty');
  mkdirSync(output, {recursive: true});
  return selected.map(([directory, extension, suffix]) => {
    const source = join(bundle, directory);
    const files = existsSync(source) ? readdirSync(source).filter(name => name.endsWith(extension) && statSync(join(source, name)).isFile()) : [];
    if (files.length !== 1) throw new Error(`Expected exactly one ${extension} in ${source}, found ${files.length}`);
    const name = `Transcriptor-${version}-${platform}-${arch}${suffix}`;
    copyFileSync(join(source, files[0]), join(output, name));
    return name;
  });
}

async function digest(path) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest('hex');
}

export async function prepareReleaseAssets(directory, version) {
  const names = readdirSync(directory).filter(name => name !== 'SHA256SUMS.txt').sort();
  const prefix = `Transcriptor-${version}-`;
  const suffixes = ['windows-x64-cpu-setup.exe', 'windows-x64-cuda-setup.exe',
    'windows-x64-cuda-portable.zip', 'linux-x64.AppImage'];
  for (const suffix of suffixes) {
    if (!names.includes(prefix + suffix)) throw new Error(`Missing release asset: ${suffix}`);
  }
  const expected = new Set(suffixes.map(suffix => prefix + suffix));
  const macNames = names.filter(name => name === prefix + 'macos-arm64.dmg' || name === prefix + 'macos-x64.dmg');
  if (macNames.length !== 1 || names.length !== 5 || names.some(name => !expected.has(name) && !macNames.includes(name))) {
    throw new Error('Expected exactly the five platform release packages');
  }
  const lines = [];
  for (const name of names) {
    const path = join(directory, name);
    if (!statSync(path).isFile() || statSync(path).size === 0) throw new Error(`Empty or invalid release asset: ${name}`);
    lines.push(`${await digest(path)}  ${name}`);
  }
  writeFileSync(join(directory, 'SHA256SUMS.txt'), lines.join('\n') + '\n');
  return [...names, 'SHA256SUMS.txt'];
}

export async function publishRelease({directory, metadata, repository, commit, token,
  api = 'https://api.github.com', request = fetch}) {
  if (!token || !/^[\w.-]+\/[\w.-]+$/.test(repository ?? '') || !/^[a-f0-9]{40}$/.test(commit ?? '')) {
    throw new Error('Publishing requires a token, repository and full commit SHA');
  }
  const names = await prepareReleaseAssets(directory, metadata.version);
  const route = `/repos/${repository}`;
  const headers = {Authorization: `Bearer ${token}`, Accept: 'application/vnd.github+json',
    'X-GitHub-Api-Version': '2026-03-10'};
  async function json(path, method = 'GET', body) {
    const result = await request(api + path, {method, headers: {...headers, 'Content-Type': 'application/json'},
      ...(body ? {body: JSON.stringify(body)} : {})});
    if (result.status === 404 && method === 'GET') return null;
    if (!result.ok) throw new Error(`GitHub ${method} ${path} failed (${result.status})`);
    return result.status === 204 ? null : result.json();
  }
  let release = await json(`${route}/releases/tags/${encodeURIComponent(metadata.tag)}`);
  if (release) {
    if (!release.draft || !release.body?.includes(MARKER)) throw new Error('Refusing to overwrite an existing release');
    const assets = await json(`${route}/releases/${release.id}/assets?per_page=100`);
    if (assets.some(asset => !names.includes(asset.name))) throw new Error('Draft contains assets not owned by this pipeline');
    for (const asset of assets) await json(`${route}/releases/assets/${asset.id}`, 'DELETE');
  } else {
    release = await json(`${route}/releases`, 'POST', {tag_name: metadata.tag, target_commitish: commit,
      name: `Transcriptor ${metadata.version}${metadata.unsigned ? ' — unsigned desktop preview' : ''}`,
      draft: true, prerelease: metadata.prerelease,
      body: `${MARKER}\n\nPackaged desktop builds for macOS, Windows and Linux.\n\n` +
        (metadata.unsigned ? '**Unsigned preview for testing.** Code signing and notarization have not been performed.\n\n' : '') +
        'Windows: choose the CPU installer, or the CUDA installer/portable ZIP for a supported NVIDIA GPU with an existing compatible driver.\n\n' +
        'Installers use per-user locations. The portable CUDA ZIP requires WebView2.\n\n' +
        'Verify downloads against SHA256SUMS.txt. See the README for requirements and installation instructions.'});
  }
  // A failed upload leaves a draft, never a public release with missing packages.
  for (const name of names) {
    const path = join(directory, name);
    const response = await request(`${release.upload_url.replace(/\{.*$/, '')}?name=${encodeURIComponent(name)}`, {
      method: 'POST', headers: {...headers, 'Content-Type': 'application/octet-stream', 'Content-Length': String(statSync(path).size)},
      body: createReadStream(path), duplex: 'half',
    });
    if (!response.ok) throw new Error(`Uploading ${name} failed (${response.status}); release remains a draft`);
    const uploaded = await response.json();
    if (uploaded.state !== 'uploaded' || uploaded.size !== statSync(path).size) {
      throw new Error(`Upload verification failed for ${name}; release remains a draft`);
    }
  }
  const published = await json(`${route}/releases/${release.id}`, 'PATCH', {
    draft: false, ...(metadata.unsigned ? {make_latest: 'false'} : {}),
  });
  return published.html_url;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const metadata = releaseMetadata(ROOT, process.env.GITHUB_REF);
    const [command, ...args] = process.argv.slice(2);
    if (command === 'metadata') {
      const values = {...metadata, matrix: JSON.stringify(releaseMatrix(args.includes('--cuda-only')))};
      const text = Object.entries(values).map(([key, value]) => `${key}=${value}`).join('\n') + '\n';
      if (process.env.GITHUB_OUTPUT) writeFileSync(process.env.GITHUB_OUTPUT, text, {flag: 'a'});
      console.log(text.trim());
    } else if (command === 'clean') {
      cleanBuildOutputs(ROOT);
    } else if (command === 'stage') {
      const [platform, flavor] = args;
      const arch = (process.env.RUNNER_ARCH || process.arch).toLowerCase();
      console.log(stageAssets({bundle: join(ROOT, 'src-tauri/target/release/bundle'), output: join(ROOT, 'release-assets'),
        version: metadata.version, platform, arch, flavor}).join('\n'));
    } else if (command === 'publish' || command === 'publish-preview') {
      let publication = metadata;
      if (command === 'publish-preview') {
        publication = previewMetadata(metadata, {eventName: process.env.GITHUB_EVENT_NAME,
          ref: process.env.GITHUB_REF, runNumber: process.env.GITHUB_RUN_NUMBER,
          signed: process.env.RELEASE_SIGNING_REQUIRED !== 'false',
          cudaOnly: process.env.RELEASE_CUDA_ONLY !== 'false'});
      } else if (process.env.GITHUB_REF !== `refs/tags/${metadata.tag}`) {
        throw new Error('Only a version tag can publish a release');
      }
      const url = await publishRelease({directory: join(ROOT, 'release-assets'), metadata: publication,
        repository: process.env.GITHUB_REPOSITORY, commit: process.env.GITHUB_SHA,
        token: process.env.GH_TOKEN, api: process.env.GITHUB_API_URL});
      console.log(url);
      if (process.env.GITHUB_STEP_SUMMARY) writeFileSync(process.env.GITHUB_STEP_SUMMARY, `Published [${publication.tag}](${url})\n`, {flag: 'a'});
    } else throw new Error('Usage: node scripts/release.mjs metadata [--cuda-only] | clean | stage platform flavor | publish | publish-preview');
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
