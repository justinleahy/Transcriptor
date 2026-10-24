import test from 'node:test';
import assert from 'node:assert/strict';
import { runCommand } from './build-process.mjs';

test('reports progress during a quiet build and stops after it exits', async () => {
  let heartbeats = 0;
  await runCommand(process.execPath, ['-e', 'setTimeout(() => {}, 120)'], {
    heartbeatMs: 20, onHeartbeat: () => heartbeats++, label: 'Quiet test build',
  });
  assert.ok(heartbeats > 0);
  const count = heartbeats;
  await new Promise(resolve => setTimeout(resolve, 40));
  assert.equal(heartbeats, count);
});

test('propagates a failed build exit code', async () => {
  await assert.rejects(runCommand(process.execPath, ['-e', 'process.exit(7)']), /failed \(7\)/);
});

test('reports an executable that cannot start', async () => {
  await assert.rejects(runCommand('transcriptor-nonexistent-build-tool', []), /ENOENT/);
});
