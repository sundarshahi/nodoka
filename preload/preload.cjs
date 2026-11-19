// loadline: loaded into every Node.js and Bun process through NODE_OPTIONS / BUN_OPTIONS.
// Makes os.cpus() and os.availableParallelism() report loadline's worker cap, so tools that size
// their worker pools from the core count (Next.js, Jest, Vitest, terser, thread-loader, pnpm…)
// start fewer workers. Only counts change, never behaviour. It must never throw.
try {
  (function () {
    var env = process.env;
    if (env.LOADLINE_OFF === '1' || (process.versions && process.versions.electron)) return;
    var os = require('os');
    var home = env.LOADLINE_HOME || (env.HOME || os.homedir()) + '/.loadline';
    if (require('fs').existsSync(home + '/off')) return;
    var cpus = os.cpus;
    var real = typeof os.availableParallelism === 'function' ? os.availableParallelism() : cpus.call(os).length;
    var n = parseInt(env.LOADLINE_CPUS, 10);
    if (!(n > 0)) n = Math.max(2, Math.floor(real / 2)); // at least 2: tools compute cpus - 1
    if (n >= real) return;
    os.cpus = function () { return cpus.apply(os, arguments).slice(0, n); };
    os.availableParallelism = function () { return n; };
    try { require('module').syncBuiltinESMExports(); } catch (e) {}
  })();
} catch (e) {}
