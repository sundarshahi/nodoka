// nodoka: loaded into every Node.js and Bun process through NODE_OPTIONS / BUN_OPTIONS.
// Makes os.cpus() and os.availableParallelism() report nodoka's worker cap, so tools that size
// their worker pools from the core count (Next.js, Jest, Vitest, terser, thread-loader, pnpm…)
// start fewer workers. Only counts change, never behaviour. It must never throw.
try {
  (function () {
    var env = process.env;
    if (env.NODOKA_OFF === '1' || (process.versions && process.versions.electron)) return;
    var os = require('os');
    var home = env.NODOKA_HOME || (env.HOME || os.homedir()) + '/.nodoka';
    if (require('fs').existsSync(home + '/off')) return;
    var cpus = os.cpus;
    var real = typeof os.availableParallelism === 'function' ? os.availableParallelism() : cpus.call(os).length;
    var n = parseInt(env.NODOKA_CPUS, 10);
    if (!(n > 0)) n = Math.max(2, Math.floor(real / 2)); // at least 2: tools compute cpus - 1
    // While the machine is short of memory (the helper writes the level every few seconds), every
    // Node tool that starts uses the fewest workers: a real `next build` peaked at ~0.6 GB, not 1.1.
    try {
      var fs = require('fs'), p = home + '/pressure';
      if (Date.now() - fs.statSync(p).mtimeMs < 30000 && parseInt(fs.readFileSync(p, 'utf8'), 10) >= 2) n = Math.min(n, 2);
    } catch (e) {}
    if (n >= real) return;
    os.cpus = function () { return cpus.apply(os, arguments).slice(0, n); };
    os.availableParallelism = function () { return n; };
    try { require('module').syncBuiltinESMExports(); } catch (e) {}
  })();
} catch (e) {}
