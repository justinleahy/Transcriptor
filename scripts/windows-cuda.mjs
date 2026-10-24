// Dependency collection is platform-independent so packaging rules can be tested
// without a Windows GPU. Only the build/loader smoke test require Windows.
import { createHash } from 'node:crypto';
import { closeSync, copyFileSync, existsSync, mkdirSync, openSync, readFileSync, readSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { runCommand } from './build-process.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CUDA_DLLS = ['cudart64_12.dll', 'cublas64_12.dll', 'cublaslt64_12.dll'];
const SYSTEM_DLLS = new Set((
  'advapi32 avrt bcrypt bcryptprimitives cabinet cfgmgr32 clbcatq combase comctl32 comdlg32 ' +
  'crypt32 cryptbase cryptnet cryptsp d2d1 d3d11 d3d12 dcomp dbghelp dnsapi dwmapi dwrite dxcore dxgi ' +
  'gdi32 gdi32full hid imm32 iphlpapi kernel32 kernelbase mmdevapi mpr msimg32 msvcrt mswsock ' +
  'ncrypt netapi32 normaliz nsi ntdll ntmarta ole32 oleacc oleaut32 opengl32 powrprof profapi propsys ' +
  'psapi rpcrt4 secur32 setupapi shell32 shlwapi sspicli ucrtbase urlmon user32 userenv uxtheme ' +
  'version win32u windowscodecs winhttp wininet winmm winmmbase wintrust wldap32 ws2_32 wtsapi32'
).split(' ').map(name => `${name}.dll`));
SYSTEM_DLLS.add('winspool.drv');
const systemImport = name => SYSTEM_DLLS.has(name) || /^(api-ms-win-|ext-ms-win-)[a-z0-9-]+\.dll$/.test(name);
const sha256 = path => {
  const hash = createHash('sha256'), chunk = Buffer.alloc(1024 * 1024), fd = openSync(path, 'r');
  try {
    let length;
    while ((length = readSync(fd, chunk)) > 0) hash.update(chunk.subarray(0, length));
    return hash.digest('hex');
  } finally { closeSync(fd); }
};

export function inspectPE(buffer) {
  const need = (offset, length) => {
    if (!Number.isSafeInteger(offset) || offset < 0 || offset + length > buffer.length) throw new Error('Truncated or invalid PE file');
  };
  need(0, 64);
  if (buffer.toString('ascii', 0, 2) !== 'MZ') throw new Error('Not a PE file');
  const pe = buffer.readUInt32LE(0x3c); need(pe, 24);
  if (buffer.toString('ascii', pe, pe + 4) !== 'PE\0\0') throw new Error('Invalid PE signature');
  if (buffer.readUInt16LE(pe + 4) !== 0x8664) throw new Error('Windows CUDA requires x64 binaries');
  const count = buffer.readUInt16LE(pe + 6), optionalSize = buffer.readUInt16LE(pe + 20), o = pe + 24;
  need(o, optionalSize);
  if (optionalSize < 112 || buffer.readUInt16LE(o) !== 0x20b || count > 96) throw new Error('Invalid x64 PE header');
  const sections = o + optionalSize; need(sections, count * 40);
  const rva = address => {
    if (address < buffer.readUInt32LE(o + 60)) { need(address, 1); return address; }
    for (let i = 0; i < count; i++) {
      const s = sections + i * 40, start = buffer.readUInt32LE(s + 12), size = buffer.readUInt32LE(s + 16);
      if (address >= start && address - start < size) {
        const offset = buffer.readUInt32LE(s + 20) + address - start; need(offset, 1); return offset;
      }
    }
    throw new Error('PE import points outside file sections');
  };
  const dllName = address => {
    const start = rva(address), end = buffer.indexOf(0, start);
    if (end < start || end - start > 255) throw new Error('Invalid DLL name');
    const name = buffer.toString('ascii', start, end).toLowerCase();
    if (!/^[a-z0-9_.-]+\.(dll|drv)$/.test(name)) throw new Error(`Unsafe DLL name: ${name}`);
    return name;
  };
  const imports = [];
  for (const [index, stride, nameOffset, delayed] of [[1, 20, 12, false], [13, 32, 4, true]]) {
    if (buffer.readUInt32LE(o + 108) <= index) continue;
    if (112 + (index + 1) * 8 > optionalSize) throw new Error('Truncated PE directories');
    const address = buffer.readUInt32LE(o + 112 + index * 8), size = buffer.readUInt32LE(o + 116 + index * 8);
    if (!address) continue;
    const start = rva(address), descriptors = Math.min(Math.floor(size / stride), 4096);
    let terminated = false;
    for (let i = 0; i < descriptors; i++) {
      const offset = start + i * stride; need(offset, stride);
      if (buffer.subarray(offset, offset + stride).every(byte => byte === 0)) { terminated = true; break; }
      if (delayed && buffer.readUInt32LE(offset) !== 1) throw new Error('Unsupported PE delay-import address format');
      imports.push({ name: dllName(buffer.readUInt32LE(offset + nameOffset)), delayed });
    }
    if (!terminated) throw new Error('Unterminated PE imports');
  }
  // cuBLAS DLLs are hundreds of MB; decode only candidate manifest fragments.
  let text = '';
  for (const encoding of ['utf8', 'utf16le']) {
    const marker = Buffer.from('<requestedExecutionLevel', encoding);
    let offset = buffer.indexOf(marker);
    while (offset !== -1) {
      text += buffer.subarray(offset, offset + 1024).toString(encoding);
      offset = buffer.indexOf(marker, offset + marker.length);
    }
  }
  if (/<requestedExecutionLevel\b[^>]*level\s*=\s*["'](?:requireAdministrator|highestAvailable)["']/i.test(text)) {
    throw new Error('Executable requests elevation');
  }
  return { imports, asInvoker: /<requestedExecutionLevel\b[^>]*level\s*=\s*["']asInvoker["']/i.test(text) };
}

function dllIndex(directories) {
  const index = new Map();
  for (const dir of directories) {
    if (!existsSync(dir)) continue;
    for (const entry of readdirSync(dir, {withFileTypes: true})) {
      if (!entry.isFile() || !/\.dll$/i.test(entry.name)) continue;
      const key = entry.name.toLowerCase(), path = join(dir, entry.name);
      if (index.has(key) && sha256(index.get(key)) !== sha256(path)) throw new Error(`Ambiguous DLL: ${key}`);
      index.set(key, path);
    }
  }
  return index;
}

export function collectRuntime({exe, cudaBin, msvcDirs = []}) {
  const main = inspectPE(readFileSync(exe));
  if (!main.asInvoker) throw new Error('Application must declare asInvoker (no elevation)');
  if (!main.imports.some(d => d.name === 'cudart64_12.dll')) throw new Error('Application was not built with CUDA 12');
  if (main.imports.some(d => d.name === 'nvcuda.dll' && !d.delayed)) throw new Error('NVIDIA driver must be delay-loaded');
  const cuda = dllIndex([cudaBin]), msvc = dllIndex(msvcDirs);
  const queue = [...main.imports.map(d => d.name), ...CUDA_DLLS], collected = new Map();
  const external = new Set();
  for (let i = 0; i < queue.length; i++) {
    const name = queue[i];
    if (collected.has(name)) continue;
    if (name === 'nvcuda.dll' || systemImport(name)) { external.add(name); continue; }
    const path = cuda.get(name) ?? msvc.get(name);
    if (!path) throw new Error(`Missing redistributable DLL: ${name}; refusing a package that relies on PATH`);
    const info = inspectPE(readFileSync(path));
    collected.set(name, {name, path, sha256: sha256(path)});
    queue.push(...info.imports.map(d => d.name));
  }
  return {dlls: [...collected.values()].sort((a, b) => a.name.localeCompare(b.name)), external: [...external].sort()};
}

export function stageRuntime({exe, cudaBin, msvcDirs, output, licenses}) {
  const result = collectRuntime({exe, cudaBin, msvcDirs});
  if (!licenses?.length || licenses.some(path => !existsSync(path))) throw new Error('CUDA redistribution licenses are required');
  mkdirSync(output, {recursive: true});
  if (readdirSync(output).length) throw new Error('Runtime staging directory must be empty');
  copyFileSync(exe, join(output, 'transcriptor.exe'));
  const resources = {};
  for (const dll of result.dlls) {
    const target = join(output, dll.name); copyFileSync(dll.path, target); resources[target] = dll.name;
  }
  for (const [i, path] of licenses.entries()) {
    const name = `LICENSE-CUDA-${i + 1}.txt`, target = join(output, name);
    copyFileSync(path, target); resources[target] = name;
  }
  const manifest = join(output, 'cuda-runtime.json');
  writeFileSync(manifest, JSON.stringify({cuda: '12.9.1', driverProvidedByOS: 'nvcuda.dll',
    dlls: result.dlls.map(({name, sha256}) => ({name, sha256})), external: result.external}, null, 2));
  resources[manifest] = 'cuda-runtime.json';
  const config = {bundle: {targets: ['nsis'], resources, windows: {nsis: {installMode: 'currentUser'}}}};
  return {...result, config};
}

async function run(args, env) {
  const cli = join(ROOT, 'node_modules/@tauri-apps/cli/tauri.js');
  await runCommand(process.execPath, [cli, ...args], {cwd: ROOT, env, label: `CUDA ${args[0]}`,
    onHeartbeat: () => {
      const progress = spawnSync('pwsh', ['-NoProfile', '-File', join(ROOT, 'scripts/windows-build-progress.ps1')],
        {encoding: 'utf8', timeout: 10000, windowsHide: true});
      if (progress.status === 0) console.log(progress.stdout.trim());
      else console.log('Compiler resource snapshot unavailable');
    },
  });
}

export async function build(args) {
  if (process.platform !== 'win32' || process.arch !== 'x64') throw new Error('Build Windows CUDA on a Windows x64 host');
  const toolkit = process.env.CUDA_PATH, redist = process.env.VCToolsRedistDir;
  if (!toolkit || !redist) throw new Error('CUDA_PATH and VCToolsRedistDir must be set; use build-windows-cuda.ps1');
  const env = {...process.env,
    // Override whisper-rs-sys's Unix-specific -fPIC default on Windows.
    CMAKE_CUDA_FLAGS: '-Xcompiler=/utf-8',
    CMAKE_CUDA_ARCHITECTURES: process.env.CMAKE_CUDA_ARCHITECTURES || '75-real;86-real;89-real;120-real;75-virtual',
    GGML_NATIVE: 'OFF',
  };
  const configIndex = args.indexOf('--config');
  if (configIndex >= 0 && !args[configIndex + 1]) throw new Error('--config requires a path');
  const signing = configIndex >= 0 ? ['--config', resolve(args[configIndex + 1])] : [];
  const unsigned = args.includes('--unsigned') ? ['--no-sign'] : [];
  await run(['build', '--ci', '--no-bundle', '--features', 'gpu-cuda', ...signing, ...unsigned, '--', '--locked', '--timings'], env);
  const output = join(ROOT, 'src-tauri/target/windows-cuda/portable');
  rmSync(output, {recursive: true, force: true});
  const licenseDir = process.env.CUDA_LICENSE_DIR || join(toolkit, 'licenses');
  const licenses = readdirSync(licenseDir).filter(name => /\.txt$/i.test(name)).map(name => join(licenseDir, name));
  const staged = stageRuntime({exe: join(ROOT, 'src-tauri/target/release/transcriptor.exe'),
    cudaBin: join(toolkit, 'bin'), msvcDirs: [join(redist, 'x64/Microsoft.VC143.CRT')], output, licenses});
  const configPath = join(ROOT, 'src-tauri/target/windows-cuda/tauri.cuda.json');
  writeFileSync(configPath, JSON.stringify(staged.config, null, 2));
  await run(['bundle', '--ci', '--features', 'gpu-cuda', '--config', configPath, ...signing, ...unsigned], env);
  // Tauri signs the executable while bundling. Use that same binary in the ZIP.
  copyFileSync(join(ROOT, 'src-tauri/target/release/transcriptor.exe'), join(output, 'transcriptor.exe'));
  const smokeEnv = {...process.env};
  for (const key of Object.keys(smokeEnv)) if (/^CUDA_/i.test(key) || /^PATH$/i.test(key)) delete smokeEnv[key];
  smokeEnv.PATH = `${process.env.SystemRoot}\\System32;${process.env.SystemRoot}`;
  const smoke = spawnSync(join(output, 'transcriptor.exe'), ['--check-runtime'], {cwd: output, env: smokeEnv, timeout: 30000});
  if (smoke.error || smoke.status !== 0) throw new Error(`Packaged runtime failed with CUDA removed from PATH (${smoke.error || smoke.status})`);
  console.log(`Windows CUDA runtime verified: ${staged.dlls.length} app-local DLLs; per-user installer and portable folder: ${output}`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    if (process.argv[2] === 'build') await build(process.argv.slice(3));
    else if (process.argv[2] === 'inspect') console.log(JSON.stringify(inspectPE(readFileSync(process.argv[3])), null, 2));
    else throw new Error('Usage: node scripts/windows-cuda.mjs build [--unsigned] [--config signing.json] | inspect file.exe');
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
