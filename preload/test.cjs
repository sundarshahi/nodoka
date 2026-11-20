// Checks the preload inside real Node processes. Run: node preload/test.cjs
const { execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const preload = path.join(__dirname, 'preload.cjs');
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'nodoka-preload-'));
const real = os.availableParallelism();
const count = (env, esm) =>
  Number(
    execFileSync(
      process.execPath,
      esm
        ? ['--input-type=module', '-e', "import { availableParallelism } from 'node:os'; console.log(availableParallelism())"]
        : ['-p', "require('os').availableParallelism()"],
      { env: { ...process.env, NODOKA_HOME: home, NODE_OPTIONS: `--require="${preload}" --max-semi-space-size=16`, ...env } },
    ),
  );
const check = (what, got, want) => {
  console.log(`${got === want ? 'ok  ' : 'FAIL'} ${what}: ${got}${got === want ? '' : `, expected ${want}`}`);
  if (got !== want) process.exitCode = 1;
};

check('NODOKA_CPUS caps the core count', count({ NODOKA_CPUS: '1' }), 1);
check('… for ESM named imports too (Vitest)', count({ NODOKA_CPUS: '1' }, true), 1);
fs.writeFileSync(path.join(home, 'pressure'), '2');
check('memory pressure → 2 workers', count({ NODOKA_CPUS: '3' }), Math.min(2, real));
const old = new Date(Date.now() - 60_000);
fs.utimesSync(path.join(home, 'pressure'), old, old);
check('a stale pressure file (helper stopped) is ignored', count({ NODOKA_CPUS: '3' }), Math.min(3, real));
fs.writeFileSync(path.join(home, 'off'), '');
check('`nodoka off` leaves Node alone', count({ NODOKA_CPUS: '1' }), real);
fs.rmSync(home, { recursive: true });
