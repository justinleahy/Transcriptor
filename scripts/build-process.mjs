import { spawn } from 'node:child_process';

// Keep long native builds observable without buffering their normal output.
export function runCommand(file, args, {cwd, env = process.env, label = 'Build',
  heartbeatMs = 60000, onHeartbeat = () => {}} = {}) {
  const started = Date.now();
  console.log(`${label}: started`);
  return new Promise((resolve, reject) => {
    const child = spawn(file, args, {cwd, env, stdio: 'inherit', windowsHide: true});
    let finished = false;
    const heartbeat = setInterval(() => {
      console.log(`${label}: still running after ${Math.round((Date.now() - started) / 1000)}s`);
      try { onHeartbeat(); } catch (error) { console.log(`Build diagnostics unavailable: ${error.message}`); }
    }, heartbeatMs);
    const finish = error => {
      if (finished) return;
      finished = true; clearInterval(heartbeat);
      if (error) reject(error);
      else {
        console.log(`${label}: completed in ${Math.round((Date.now() - started) / 1000)}s`);
        resolve();
      }
    };
    child.once('error', finish);
    child.once('close', (code, signal) => finish(code === 0 ? null : new Error(`${label} failed (${signal || code})`)));
  });
}
