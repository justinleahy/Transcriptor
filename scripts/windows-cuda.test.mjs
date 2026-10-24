import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { inspectPE, collectRuntime, stageRuntime } from './windows-cuda.mjs';

function pe(imports = [], delayed = [], machine = 0x8664) {
  const b = Buffer.alloc(0x1200);
  b.write('MZ'); b.writeUInt32LE(0x80, 0x3c); b.write('PE\0\0', 0x80);
  b.writeUInt16LE(machine, 0x84); b.writeUInt16LE(1, 0x86); b.writeUInt16LE(240, 0x94);
  const o = 0x98;
  b.writeUInt16LE(0x20b, o); b.writeBigUInt64LE(0x140000000n, o + 24);
  b.writeUInt32LE(0x200, o + 60); b.writeUInt32LE(16, o + 108);
  b.writeUInt32LE(0x1000, o + 112 + 8); b.writeUInt32LE((imports.length + 1) * 20, o + 116 + 8);
  b.writeUInt32LE(0x1200, o + 112 + 13 * 8); b.writeUInt32LE((delayed.length + 1) * 32, o + 116 + 13 * 8);
  const s = o + 240;
  b.write('.rdata', s); b.writeUInt32LE(0x1000, s + 8); b.writeUInt32LE(0x1000, s + 12);
  b.writeUInt32LE(0x1000, s + 16); b.writeUInt32LE(0x200, s + 20);
  let name = 0x600;
  for (const [i, dll] of imports.entries()) {
    b.writeUInt32LE(name + 0xe00, 0x200 + i * 20 + 12); b.write(dll + '\0', name); name += dll.length + 1;
  }
  for (const [i, dll] of delayed.entries()) {
    b.writeUInt32LE(1, 0x400 + i * 32); b.writeUInt32LE(name + 0xe00, 0x400 + i * 32 + 4);
    b.write(dll + '\0', name); name += dll.length + 1;
  }
  b.write('<requestedExecutionLevel level="asInvoker" uiAccess="false"/>', 0x1000);
  return b;
}

test('reads normal and delay-loaded Windows imports', () => {
  assert.deepEqual(inspectPE(pe(['KERNEL32.dll', 'cudart64_12.dll'], ['nvcuda.dll'])).imports, [
    {name: 'kernel32.dll', delayed: false}, {name: 'cudart64_12.dll', delayed: false},
    {name: 'nvcuda.dll', delayed: true},
  ]);
});

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'transcriptor-cuda-test-'));
  t.after(() => rmSync(root, {recursive: true, force: true}));
  const cudaBin = join(root, 'cuda'); const crt = join(root, 'crt');
  mkdirSync(cudaBin); mkdirSync(crt);
  const exe = join(root, 'transcriptor.exe');
  writeFileSync(exe, pe(['kernel32.dll', 'cudart64_12.dll', 'cublas64_12.dll'], ['nvcuda.dll']));
  writeFileSync(join(cudaBin, 'cudart64_12.dll'), pe(['kernel32.dll', 'vcruntime140.dll']));
  writeFileSync(join(cudaBin, 'cublas64_12.dll'), pe(['cublasLt64_12.dll']));
  writeFileSync(join(cudaBin, 'cublasLt64_12.dll'), pe(['msvcp140.dll']));
  for (const name of ['vcruntime140.dll', 'msvcp140.dll']) writeFileSync(join(crt, name), pe(['kernel32.dll']));
  return {root, exe, cudaBin, msvcDirs: [crt]};
}

test('stages transitive CUDA and MSVC DLLs beside the exe, never the driver', t => {
  const f = fixture(t);
  const license = join(f.root, 'license.txt'); writeFileSync(license, 'Redistribution license');
  const result = stageRuntime({...f, output: join(f.root, 'package'), licenses: [license]});
  assert.deepEqual(result.dlls.map(x => x.name), ['cublas64_12.dll', 'cublaslt64_12.dll', 'cudart64_12.dll', 'msvcp140.dll', 'vcruntime140.dll']);
  assert.ok(result.dlls.every(x => /^[a-f0-9]{64}$/.test(x.sha256)));
  assert.equal(Object.values(result.config.bundle.resources).includes('nvcuda.dll'), false);
  assert.ok(Object.values(result.config.bundle.resources).includes('cudart64_12.dll'));
  assert.equal(result.config.bundle.windows.nsis.installMode, 'currentUser');
  const manifest = JSON.parse(readFileSync(join(f.root, 'package/cuda-runtime.json')));
  for (const dll of manifest.dlls) {
    assert.equal(dll.sha256, createHash('sha256').update(readFileSync(join(f.root, 'package', dll.name))).digest('hex'));
  }
});

test('fails instead of relying on a CUDA DLL from the developer PATH', t => {
  const f = fixture(t); rmSync(join(f.cudaBin, 'cublasLt64_12.dll'));
  assert.throws(() => collectRuntime(f), /cublaslt64_12.dll/);
});

test('rejects mixed architectures and elevated executables', t => {
  const f = fixture(t); writeFileSync(join(f.cudaBin, 'cublasLt64_12.dll'), pe([], [], 0x14c));
  assert.throws(() => collectRuntime(f), /x64/);
  const elevated = pe(); elevated.write('<requestedExecutionLevel level="requireAdministrator"/>', 0x1000);
  assert.throws(() => inspectPE(elevated), /elevation/);
});

test('rejects malformed import names and truncated PE files', () => {
  assert.throws(() => inspectPE(pe(['../evil.dll'])), /name/);
  assert.throws(() => inspectPE(Buffer.from('MZ')), /PE/);
});

test('requires a CUDA executable, explicit non-elevation and delay-loaded driver', t => {
  const f = fixture(t);
  writeFileSync(f.exe, pe(['kernel32.dll']));
  assert.throws(() => collectRuntime(f), /not built with CUDA/);
  const missingManifest = pe(['cudart64_12.dll']); missingManifest.fill(0, 0x1000);
  writeFileSync(f.exe, missingManifest);
  assert.throws(() => collectRuntime(f), /asInvoker/);
  writeFileSync(f.exe, pe(['cudart64_12.dll', 'nvcuda.dll']));
  assert.throws(() => collectRuntime(f), /delay-loaded/);
});

test('rejects an unknown dependency rather than assuming Windows provides it', t => {
  const f = fixture(t);
  writeFileSync(join(f.cudaBin, 'cublasLt64_12.dll'), pe(['unexpected.dll']));
  assert.throws(() => collectRuntime(f), /unexpected.dll/);
});

test('requires redistribution licenses and refuses to overwrite existing output', t => {
  const f = fixture(t), output = join(f.root, 'package');
  assert.throws(() => stageRuntime({...f, output, licenses: []}), /licenses/);
  const license = join(f.root, 'license.txt'); writeFileSync(license, 'License');
  stageRuntime({...f, output, licenses: [license]});
  assert.throws(() => stageRuntime({...f, output, licenses: [license]}), /must be empty/);
});

test('rejects conflicting copies of an MSVC DLL', t => {
  const f = fixture(t), extra = join(f.root, 'extra'); mkdirSync(extra);
  writeFileSync(join(extra, 'vcruntime140.dll'), pe(['user32.dll']));
  assert.throws(() => collectRuntime({...f, msvcDirs: [...f.msvcDirs, extra]}), /Ambiguous DLL/);
});

test('rejects an unterminated delay import directory', () => {
  const b = pe([], ['nvcuda.dll']); b.writeUInt32LE(32, 0x98 + 116 + 13 * 8);
  assert.throws(() => inspectPE(b), /Unterminated PE imports/);
});

// Optional cross-platform integration check after setup-windows-cuda.ps1. The
// executable is a PE fixture; DLL bytes and licenses come from NVIDIA archives.
if (process.env.TRANSCRIPTOR_CUDA_TEST_TOOLKIT) {
  test('stages actual NVIDIA redistributables and preserves their checksums', t => {
    const f = fixture(t), toolkit = process.env.TRANSCRIPTOR_CUDA_TEST_TOOLKIT;
    const result = stageRuntime({...f, cudaBin: join(toolkit, 'bin'), output: join(f.root, 'package'),
      licenses: [join(toolkit, 'licenses/cuda_cudart-LICENSE.txt'), join(toolkit, 'licenses/libcublas-LICENSE.txt')]});
    assert.deepEqual(result.dlls.map(x => x.name), ['cublas64_12.dll', 'cublaslt64_12.dll', 'cudart64_12.dll']);
    for (const dll of result.dlls) {
      assert.equal(dll.sha256, createHash('sha256').update(readFileSync(join(f.root, 'package', dll.name))).digest('hex'));
    }
  });
}
