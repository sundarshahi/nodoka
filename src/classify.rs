//! Recognise dev work from a process's command line, for any stack: installs, builds, tests and
//! long-running dev processes (servers and watchers). Unknown commands are left alone.

use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Install,
    Build,
    Test,
    Dev,
}

impl Kind {
    /// Heavy jobs are admitted a few at a time; dev processes are not.
    pub fn heavy(self) -> bool {
        self != Kind::Dev
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Install => "install",
            Kind::Build => "build",
            Kind::Test => "test",
            Kind::Dev => "dev",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "install" => Kind::Install,
            "build" => Kind::Build,
            "test" => Kind::Test,
            "dev" => Kind::Dev,
            _ => return None,
        })
    }
}

/// Extra rules from the config file: tool name → kind, or `None` to never touch the tool.
pub type Custom = HashMap<String, Option<Kind>>;

pub fn classify(argv: &[String], custom: &Custom) -> Option<Kind> {
    let argv = untitle(argv);
    let (tool, rest) = resolve(&argv, 0)?;
    if let Some(k) = custom.get(&tool) {
        return *k;
    }
    rule(&tool, &rest)
}

/// Program name as rules see it: no directory, extension or version suffix, lower case.
pub fn stem(s: &str) -> String {
    let base = s.rsplit(['/', '\\']).next().unwrap_or(s).trim_end_matches(':').to_ascii_lowercase();
    if base == "py.test" {
        return "pytest".into();
    }
    let base = if base.starts_with('.') { base.as_str() } else { base.split('.').next().unwrap_or(&base) };
    let versioned =
        |name: &str| base.strip_prefix(name).is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '-'));
    for (name, canon) in [("python", "python"), ("pypy", "python"), ("ruby", "ruby"), ("php", "php"), ("node", "node")]
    {
        if versioned(name) {
            return canon.into();
        }
    }
    match base {
        "nodejs" => "node",
        "npm-cli" => "npm",
        "npx-cli" => "npx",
        "yarnpkg" => "yarn",
        "bundler" => "bundle",
        "sbt-launch" => "sbt",
        "flutter_tools" => "flutter",
        "gmake" | "bmake" => "make",
        b if b.starts_with("yarn-") => "yarn",
        b => b,
    }
    .into()
}

/// Servers like puma and gunicorn rewrite their process title into argv[0]; split it back into words.
pub fn untitle(argv: &[String]) -> Vec<String> {
    match argv.split_first() {
        Some((first, rest)) if first.contains(' ') && rest.iter().all(|a| a.trim().is_empty()) => {
            first.split_whitespace().map(String::from).collect()
        }
        _ => argv.to_vec(),
    }
}

/// A tool that renamed itself to a bare name (webpack-cli sets its title to just "webpack") wiped
/// its arguments, so `webpack serve` would look like a build. When a package script started it,
/// npm, yarn, pnpm and bun leave the command in `npm_lifecycle_script`: take the tool's arguments
/// from there.
pub fn unwipe(argv: Vec<String>, script: Option<&str>) -> Vec<String> {
    let (Some(script), Some(first)) = (script, argv.first()) else { return argv };
    if first.trim().contains(' ') || !argv[1..].iter().all(|a| a.trim().is_empty()) {
        return argv;
    }
    let tool = stem(first.trim());
    let words: Vec<&str> = script.split_whitespace().collect();
    match words.iter().position(|w| stem(w) == tool) {
        Some(i) => words[i..]
            .iter()
            .take_while(|w| !matches!(**w, "&&" | "||" | ";" | "|" | "&"))
            .map(|w| w.to_string())
            .collect(),
        None => argv,
    }
}

const SHELL_RUNNERS: &[&str] = &["node", "ruby", "python", "php", "perl"];

/// Find the tool doing the work: look through interpreters (node, ruby, python, java, beam…)
/// and wrappers (npx, bundle exec, uv run…). Returns the tool name and its arguments.
fn resolve(argv: &[String], depth: u8) -> Option<(String, Vec<String>)> {
    if depth > 4 {
        return None;
    }
    let (first, rest) = argv.split_first()?;
    let tool = stem(first);
    let rest = rest.to_vec();

    if SHELL_RUNNERS.contains(&tool.as_str()) {
        return script(&tool, &rest, depth);
    }
    match tool.as_str() {
        "java" => return java(&rest),
        "beam" => {
            // Erlang VM: `… -extra /usr/local/bin/mix phx.server` or `iex -S mix phx.server`
            let i = rest.iter().rposition(|a| matches!(stem(a).as_str(), "mix" | "rebar3"))?;
            return Some((stem(&rest[i]), rest[i + 1..].to_vec()));
        }
        "dart" | "bun" | "deno" | "dotnet" => {
            // runtimes that have subcommands and also run files directly
            let pos = rest.iter().position(|a| !a.starts_with('-'));
            let known = runtime_subcommand(&tool, pos.map(|i| rest[i].as_str()).unwrap_or(""));
            if let (Some(i), false) = (pos, known) {
                if tool == "bun" || tool == "dart" {
                    let watch = rest[..i].iter().any(|a| matches!(a.as_str(), "--watch" | "--hot"));
                    let r = resolve(&rest[i..], depth + 1);
                    return if watch && r.as_ref().is_none_or(|(t, a)| rule(t, a).is_none()) {
                        Some(("--watch".into(), vec![]))
                    } else {
                        r
                    };
                }
                return None;
            }
        }
        _ => {}
    }

    // wrappers that run another command
    let sub_at = rest.iter().position(|a| !a.starts_with('-'));
    let sub = sub_at.map(|i| rest[i].as_str()).unwrap_or("");
    let after = |i: Option<usize>| -> Vec<String> {
        let tail = &rest[i.map_or(rest.len(), |i| i + 1)..];
        let tail = if tail.first().is_some_and(|a| a == "--") { &tail[1..] } else { tail };
        skip_flags(tail)
    };
    let inner = match (tool.as_str(), sub) {
        ("npx" | "bunx" | "pnpx" | "uvx", _) => Some(skip_flags(&rest)),
        ("npm", "exec" | "x")
        | ("pnpm", "exec" | "dlx")
        | ("yarn", "exec" | "dlx")
        | ("bundle", "exec")
        | ("bun", "x" | "run") => Some(after(sub_at)),
        ("uv" | "poetry" | "pipenv" | "pdm" | "rye" | "hatch" | "pixi", "run") | ("mise", "exec" | "x") => {
            Some(after(sub_at))
        }
        ("uv", "tool") if sub_at.and_then(|i| rest.get(i + 1)).is_some_and(|a| a == "run") => {
            Some(after(sub_at.map(|i| i + 1)))
        }
        ("env" | "time", _) => {
            Some(rest.iter().skip_while(|a| a.starts_with('-') || a.contains('=')).cloned().collect())
        }
        _ => None,
    };
    match inner {
        Some(inner) if !inner.is_empty() => resolve(&inner, depth + 1),
        _ => Some((tool, rest)),
    }
}

fn skip_flags(args: &[String]) -> Vec<String> {
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        i += if matches!(args[i].as_str(), "-p" | "--package" | "--from" | "--with" | "-c" | "--call") { 2 } else { 1 };
    }
    args.get(i..).unwrap_or(&[]).to_vec()
}

fn runtime_subcommand(tool: &str, sub: &str) -> bool {
    let subs: &[&str] = match tool {
        "bun" => &[
            "install", "i", "add", "a", "remove", "rm", "update", "outdated", "test", "build", "run", "x", "create",
            "init", "upgrade", "pm", "link", "unlink", "publish", "exec", "repl",
        ],
        "deno" => &[
            "run", "serve", "task", "test", "bench", "compile", "check", "install", "cache", "add", "lint", "fmt",
            "doc", "repl", "eval", "jupyter", "upgrade",
        ],
        "dotnet" => &[
            "watch", "run", "build", "publish", "pack", "test", "vstest", "restore", "msbuild", "clean", "format",
            "add", "tool", "new", "ef",
        ],
        "dart" => &["run", "test", "compile", "pub", "analyze", "format", "fix", "create", "doc"],
        _ => &[],
    };
    sub.is_empty() || subs.contains(&sub)
}

/// `node|ruby|python|php|perl [flags] script args` → the script is the tool.
fn script(interp: &str, args: &[String], depth: u8) -> Option<(String, Vec<String>)> {
    let mut i = 0;
    let mut watch = false;
    while let Some(a) = args.get(i) {
        let a = a.as_str();
        if !a.starts_with('-') || a == "-" {
            break;
        }
        let inline_code = match interp {
            "node" => matches!(a, "-e" | "--eval" | "-p" | "--print" | "--run"),
            "ruby" => a == "-e",
            "python" => a == "-c",
            "php" => a == "-r",
            "perl" => matches!(a, "-e" | "-E"),
            _ => false,
        };
        if inline_code {
            return None;
        }
        match (interp, a) {
            ("node", "--test") => return Some(("node --test".into(), vec![])),
            ("node", w) if w.starts_with("--watch") => watch = true,
            ("python", "-m") => {
                let module = args.get(i + 1)?.to_ascii_lowercase();
                return Some((module, args[i + 2..].to_vec()));
            }
            ("ruby", "-S") => return resolve(&args[i + 1..], depth + 1),
            ("php", "-S") => return Some(("php -S".into(), vec![])),
            _ => {}
        }
        let takes_value = match interp {
            "node" => matches!(
                a,
                "-r" | "--require"
                    | "--import"
                    | "--loader"
                    | "--experimental-loader"
                    | "-C"
                    | "--conditions"
                    | "--env-file"
                    | "--title"
                    | "--watch-path"
                    | "--input-type"
            ),
            "ruby" => matches!(a, "-I" | "-r" | "-C" | "-E" | "-x"),
            "python" => matches!(a, "-X" | "-W" | "-Q"),
            "php" => matches!(a, "-d" | "-c" | "-t" | "-z" | "-f"),
            _ => false,
        };
        i += if takes_value { 2 } else { 1 };
    }
    if i >= args.len() {
        return watch.then(|| ("--watch".into(), vec![]));
    }
    // the script path is the new argv[0], so wrappers like `poetry run` resolve too
    let r = resolve(&args[i..], depth + 1);
    if watch && r.as_ref().is_none_or(|(t, a)| rule(t, a).is_none()) {
        return Some(("--watch".into(), vec![]));
    }
    r
}

/// `java [jvm flags] MainClass args` / `java -jar x.jar args`
fn java(args: &[String]) -> Option<(String, Vec<String>)> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "-jar" {
            return Some((stem(args.get(i + 1)?), args[i + 2..].to_vec()));
        }
        if !a.starts_with('-') {
            break;
        }
        let takes_value = matches!(
            a.as_str(),
            "-cp"
                | "-classpath"
                | "--class-path"
                | "-p"
                | "--module-path"
                | "--upgrade-module-path"
                | "--add-modules"
                | "--add-reads"
                | "--add-exports"
                | "--add-opens"
                | "--limit-modules"
                | "--patch-module"
                | "--source"
                | "-m"
                | "--module"
        );
        i += if takes_value { 2 } else { 1 };
    }
    let main = args.get(i)?;
    let rest = args[i + 1..].to_vec();
    let tool = match main.as_str() {
        "org.codehaus.plexus.classworlds.launcher.Launcher" => "mvn",
        "org.gradle.launcher.daemon.bootstrap.GradleDaemon" => "gradle-daemon",
        "org.jetbrains.kotlin.daemon.KotlinCompileDaemon" => "kotlin-daemon",
        "xsbt.boot.Boot" => "sbt",
        "clojure.main" if rest.iter().any(|a| a == "leiningen.core.main") => {
            let at = rest.iter().position(|a| a == "leiningen.core.main")?;
            return Some(("lein".into(), rest[at + 1..].to_vec()));
        }
        // an app's own main class: no built-in rule, but `dev = application` in the config can name it
        other => {
            return Some((
                other.rsplit(['.', '/']).find(|s| !s.is_empty() && *s != "java").unwrap_or(other).to_ascii_lowercase(),
                rest,
            ));
        }
    };
    Some((tool.into(), rest))
}

/// First matching group wins. `""` in a group matches "no subcommand given".
fn pick(word: &str, groups: &[(&[&str], Kind)]) -> Option<Kind> {
    groups.iter().find(|(words, _)| words.contains(&word)).map(|g| g.1)
}

/// Look at every argument (task runners like turbo/nx/make take several targets).
fn scan(args: &[&str], groups: &[(&[&str], Kind)], split: bool) -> Option<Kind> {
    for (words, kind) in groups {
        let hit = args
            .iter()
            .any(|a| if split { a.split([':', '#', ',', '/']).any(|p| words.contains(&p)) } else { words.contains(a) });
        if hit {
            return Some(*kind);
        }
    }
    None
}

const TASK_WORDS: &[(&[&str], Kind)] = &[
    (&["dev", "develop", "start", "serve", "server", "watch", "preview"], Kind::Dev),
    (&["test", "tests", "e2e", "spec"], Kind::Test),
    (
        &[
            "build",
            "compile",
            "lint",
            "check",
            "typecheck",
            "check-types",
            "tsc",
            "bundle",
            "package",
            "dist",
            "release",
        ],
        Kind::Build,
    ),
    (&["install", "deps", "setup", "bootstrap"], Kind::Install),
];

pub fn rule(tool: &str, rest: &[String]) -> Option<Kind> {
    use Kind::*;
    let pos: Vec<&str> = rest.iter().map(String::as_str).filter(|a| !a.starts_with('-')).collect();
    let sub = pos.first().copied().unwrap_or("");
    let sub2 = pos.get(1).copied().unwrap_or("");
    let has = |w: &str| rest.iter().any(|a| a == w);
    let watch = rest.iter().any(|a| {
        matches!(a.as_str(), "-w" | "--watch" | "--watchAll" | "--watch-all" | "--serve" | "--hot" | "--live")
            || a.starts_with("--watch=")
    });
    let dev_or = |k: Kind| Some(if watch { Dev } else { k });

    match tool {
        // ---------------- JavaScript / TypeScript
        "npm" => pick(
            sub,
            &[(
                &[
                    "install",
                    "i",
                    "in",
                    "ins",
                    "inst",
                    "insta",
                    "instal",
                    "isnt",
                    "isnta",
                    "isntal",
                    "isntall",
                    "add",
                    "ci",
                    "clean-install",
                    "ic",
                    "install-clean",
                    "isntall-clean",
                    "update",
                    "up",
                    "upgrade",
                    "udpate",
                    "rebuild",
                    "rb",
                    "install-test",
                    "it",
                    "install-ci-test",
                    "cit",
                ],
                Install,
            )],
        ),
        "pnpm" => pick(
            sub,
            &[(&["install", "i", "add", "update", "up", "upgrade", "rebuild", "rb", "fetch", "import"], Install)],
        ),
        "yarn" => pick(sub, &[(&["", "install", "add", "up", "upgrade", "upgrade-interactive", "import"], Install)]),
        "bun" => {
            pick(sub, &[(&["install", "i", "add", "a", "update"], Install), (&["test"], Test), (&["build"], Build)])
        }
        "deno" => match sub {
            "test" | "bench" => Some(Test),
            "compile" | "check" | "lint" => Some(Build),
            "install" | "cache" | "add" => Some(Install),
            "serve" => Some(Dev),
            "run" if watch => Some(Dev),
            _ => None,
        },
        "node --test" => Some(Test),
        "--watch" => Some(Dev), // `node --watch app.js`, `bun --hot server.ts`
        "next" => pick(sub, &[(&["dev", "start"], Dev), (&["build", "lint", "experimental-compile"], Build)]),
        "next-server" => Some(Dev), // Next renames its processes: `next-server (v16.4.5)`…
        "next-build" => Some(Build),
        "next-router-worker" | "next-render-worker-app" | "next-render-worker-pages" => Some(Dev),
        "vite" => pick(sub, &[(&["", "dev", "serve", "preview"], Dev), (&["build", "optimize"], Build)]).or(Some(Dev)),
        "vitest" => Some(pick(sub, &[(&["watch", "dev"], Dev)]).unwrap_or(Test)),
        "jest" | "mocha" | "ava" | "jasmine" | "tap" | "uvu" | "tape" | "cucumber-js" | "karma" | "web-test-runner"
        | "wtr" => dev_or(Test),
        "playwright" => pick(sub, &[(&["test"], Test)]),
        "cypress" => pick(sub, &[(&["run"], Test)]),
        // a browser driven by a script (Playwright, Puppeteer, Selenium): heavy, with timeouts like a test.
        // Only headless ones: the browser you use never runs with --headless.
        "chrome-headless-shell" | "headless_shell" => Some(Test),
        "chrome"
        | "chromium"
        | "chromium-browser"
        | "google chrome"
        | "google chrome for testing"
        | "msedge"
        | "microsoft edge"
        | "firefox"
            if rest
                .iter()
                .any(|a| matches!(a.as_str(), "--headless" | "-headless") || a.starts_with("--headless=")) =>
        {
            Some(Test)
        }
        "webpack" | "webpack-cli" => Some(if watch || matches!(sub, "serve" | "s" | "server") { Dev } else { Build }),
        "webpack-dev-server" | "nodemon" | "ts-node-dev" | "tsnd" | "node-dev" | "serve" | "http-server"
        | "live-server" | "lite-server" | "json-server" | "browser-sync" | "sirv" | "servor" | "start-storybook"
        | "pm2-dev" | "web-dev-server" | "wds" | "react-router-serve" | "remix-serve" => Some(Dev),
        "tauri" | "electron-vite" | "ionic" | "snowpack" | "umi" => {
            pick(sub, &[(&["dev", "serve", "preview", "start"], Dev), (&["build"], Build)])
        }
        "electron-forge" => pick(sub, &[(&["start"], Dev), (&["make", "package", "publish"], Build)]),
        "electron-builder" => Some(Build),
        "ember" => pick(
            sub,
            &[
                (&["serve", "server", "s"], Dev),
                (&["build", "b"], Build),
                (&["test", "t"], Test),
                (&["install"], Install),
            ],
        ),
        "rspack" | "rsbuild" | "farm" => {
            pick(sub, &[(&["dev", "serve", "preview", "start"], Dev), (&["", "build"], Build)])
        }
        "parcel" => Some(if sub == "build" { Build } else { Dev }),
        "esbuild" | "tsup" | "tsdown" | "unbuild" | "rollup" | "rolldown" | "microbundle" | "pkgroll" | "babel"
        | "swc" | "ncc" | "tailwindcss" | "postcss" | "sass" | "lessc" | "stylus" | "tsc" | "vue-tsc" | "tsgo"
        | "svelte-check" | "eleventy" | "build-storybook" => dev_or(Build),
        "eslint" | "prettier" | "stylelint" | "oxlint" | "knip" | "publint" | "attw" | "depcheck" | "madge" => {
            dev_or(Build)
        }
        "biome" => pick(sub, &[(&["check", "lint", "format", "ci"], Build)]),
        "tsx" | "vite-node" | "ts-node" | "jiti" => {
            if watch || sub == "watch" {
                Some(Dev)
            } else {
                None
            }
        }
        "nuxi" | "nuxt" => pick(
            sub,
            &[
                (&["dev", "preview"], Dev),
                (&["build", "generate", "analyze", "typecheck", "prepare"], Build),
                (&["test"], Test),
            ],
        ),
        "astro" => pick(sub, &[(&["dev", "preview", "start"], Dev), (&["build", "check", "sync"], Build)]),
        "svelte-kit" | "remix" | "react-router" | "vinxi" | "solid-start" | "qwik" | "analog" | "waku" | "rakkas"
        | "vike" => {
            pick(sub, &[(&["dev", "start", "preview", "vite:dev"], Dev), (&["build", "vite:build", "typegen"], Build)])
        }
        "gatsby" => pick(sub, &[(&["develop", "serve"], Dev), (&["build"], Build)]),
        "expo" => pick(sub, &[(&["start"], Dev), (&["export", "prebuild", "run:ios", "run:android"], Build)]),
        "react-native" => pick(
            sub,
            &[(&["start"], Dev), (&["run-android", "run-ios", "bundle", "build-android", "build-ios"], Build)],
        ),
        "ng" => pick(
            sub,
            &[(&["serve", "s", "dev"], Dev), (&["build", "b", "lint"], Build), (&["test", "t", "e2e", "e"], Test)],
        ),
        "vue-cli-service" => {
            pick(sub, &[(&["serve"], Dev), (&["build", "lint"], Build), (&["test:unit", "test:e2e"], Test)])
        }
        "react-scripts" | "craco" | "react-app-rewired" => {
            pick(sub, &[(&["start"], Dev), (&["build"], Build), (&["test"], Test)])
        }
        "storybook" => pick(sub, &[(&["dev"], Dev), (&["build"], Build)]),
        "nest" => pick(sub, &[(&["start"], Dev), (&["build"], Build)]),
        "docusaurus" | "vitepress" | "vuepress" | "hexo" | "slidev" => {
            pick(sub, &[(&["start", "dev", "serve", "server", "s"], Dev), (&["build", "generate", "g"], Build)])
        }
        "wrangler" | "vercel" | "netlify" | "miniflare" => {
            if sub == "dev" || sub2 == "dev" {
                Some(Dev)
            } else {
                pick(sub, &[(&["build"], Build)])
            }
        }
        "firebase" => pick(sub, &[(&["emulators:start", "serve"], Dev)]),
        "prisma" => pick(sub, &[(&["studio"], Dev), (&["generate"], Build)]),
        "turbo" | "nx" | "lerna" | "moon" | "ultra" | "wireit" => scan(&pos, TASK_WORDS, true),

        // ---------------- Ruby
        "rails" => match sub {
            "server" | "s" => Some(Dev),
            "assets:precompile"
            | "tailwindcss:build"
            | "dartsass:build"
            | "javascript:build"
            | "css:build"
            | "shakapacker:compile"
            | "webpacker:compile" => Some(Build),
            "tailwindcss:watch" | "dartsass:watch" | "javascript:watch" | "css:watch" => Some(Dev),
            s if s == "t" || s.starts_with("test") || s.starts_with("spec") => Some(Test),
            _ => None,
        },
        "rake" => match sub {
            s if s.starts_with("test") || s.starts_with("spec") => Some(Test),
            "assets:precompile" => Some(Build),
            _ => None,
        },
        "rspec" | "cucumber" | "turnip" | "minitest" => Some(Test),
        "bundle" => pick(sub, &[(&["", "install", "i", "update", "lock", "cache", "package"], Install)]),
        "gem" => pick(sub, &[(&["install", "i", "update", "pristine"], Install)]),
        "pod" => pick(sub, &[(&["install", "update"], Install)]),
        "puma"
        | "unicorn"
        | "unicorn_rails"
        | "thin"
        | "falcon"
        | "rackup"
        | "iodine"
        | "pitchfork"
        | "agoo"
        | "guard"
        | "hivemind"
        | "shakapacker-dev-server"
        | "webpacker-dev-server" => Some(Dev),
        "passenger" => pick(sub, &[(&["start"], Dev)]),
        "foreman" | "honcho" | "forego" | "goreman" => pick(sub, &[(&["start", ""], Dev)]),
        "overmind" => pick(sub, &[(&["start", "s"], Dev)]),
        "jekyll" | "middleman" | "bridgetown" | "nanoc" => pick(
            sub,
            &[(&["serve", "s", "server", "start", "dev", "live", "view"], Dev), (&["build", "b", "compile"], Build)],
        ),
        "shakapacker" | "webpacker" | "rubocop" | "standardrb" | "steep" | "brakeman" | "reek" | "erb_lint"
        | "herb" => Some(Build),
        "srb" => pick(sub, &[(&["tc", "typecheck"], Build)]),

        // ---------------- Python
        "manage" | "django-admin" => pick(
            sub,
            &[
                (&["runserver", "runserver_plus", "runsslserver", "runmodwsgi"], Dev),
                (&["test"], Test),
                (&["collectstatic", "compilemessages"], Build),
            ],
        ),
        "flask" => {
            if has("run") {
                Some(Dev)
            } else {
                None
            }
        }
        "uvicorn" | "gunicorn" | "hypercorn" | "daphne" | "granian" | "waitress-serve" | "sanic" | "http.server"
        | "simplehttpserver" | "voila" | "sphinx-autobuild" | "ptw" | "pytest-watch" | "gradio" => Some(Dev),
        "fastapi" | "litestar" | "reflex" | "streamlit" | "chainlit" | "solara" => pick(sub, &[(&["dev", "run"], Dev)]),
        "panel" => pick(sub, &[(&["serve"], Dev)]),
        "mkdocs" => pick(sub, &[(&["serve"], Dev), (&["build", "gh-deploy"], Build)]),
        "pelican" => Some(if rest.iter().any(|a| matches!(a.as_str(), "--listen" | "-l" | "--autoreload" | "-r")) {
            Dev
        } else {
            Build
        }),
        "sphinx-build" | "mypy" | "pyright" | "basedpyright" | "pylint" | "flake8" | "pyre" | "pytype" | "bandit"
        | "black" | "isort" => dev_or(Build),
        "ruff" => pick(sub, &[(&["check", "format"], Build)]),
        "pytest" | "tox" | "nox" | "unittest" | "behave" | "ward" | "green" | "nose2" => Some(Test),
        "pip" | "pip3" => pick(sub, &[(&["install", "download", "wheel"], Install)]),
        "pip-compile" | "pip-sync" => Some(Install),
        "uv" => match sub {
            "sync" | "lock" | "add" | "remove" | "venv" | "pip" | "tool" | "python" => Some(Install),
            "build" => Some(Build),
            _ => None,
        },
        "poetry" | "pdm" | "rye" | "pixi" | "pipenv" | "hatch" => pick(
            sub,
            &[
                (&["install", "add", "update", "lock", "sync", "remove"], Install),
                (&["build"], Build),
                (&["test"], Test),
            ],
        ),
        "conda" | "mamba" | "micromamba" => pick(sub, &[(&["install", "create", "update", "env"], Install)]),
        "maturin" => pick(sub, &[(&["develop", "build"], Build)]),
        "setup" => pick(sub, &[(&["build", "build_ext", "bdist_wheel", "sdist", "develop", "install"], Build)]),

        // ---------------- Go
        "go" => pick(
            sub,
            &[
                (&["build", "install", "vet", "generate"], Build),
                (&["test"], Test),
                (&["run"], Dev),
                (&["mod", "get"], Install),
            ],
        ),
        "air" | "gow" | "wgo" | "compiledaemon" | "realize" | "fresh" | "modd" | "devd" | "watchexec" | "entr"
        | "bacon" | "cargo-watch" => Some(Dev),
        "templ" => Some(if watch { Dev } else { Build }),
        "wails" | "buffalo" | "encore" => pick(sub, &[(&["dev", "run"], Dev), (&["build"], Build), (&["test"], Test)]),
        "golangci-lint" => pick(sub, &[(&["run"], Build)]),
        "staticcheck" => Some(Build),
        "hugo" => pick(sub, &[(&["server", "serve"], Dev), (&["", "build"], Build)]),
        "zola" | "mdbook" => pick(sub, &[(&["serve", "watch"], Dev), (&["build", "check"], Build), (&["test"], Test)]),

        // ---------------- Rust
        "cargo" => match sub {
            "build" | "b" | "check" | "c" | "clippy" | "doc" | "d" | "install" | "fix" | "rustc" | "rustdoc" => {
                Some(Build)
            }
            "test" | "t" | "nextest" | "bench" | "miri" => Some(Test),
            "run" | "r" | "watch" | "shuttle" => Some(Dev),
            "fetch" | "update" | "vendor" | "generate-lockfile" => Some(Install),
            "leptos" | "tauri" | "lambda" => {
                pick(sub2, &[(&["watch", "serve", "dev"], Dev), (&["build"], Build), (&["test"], Test)])
            }
            _ => None,
        },
        "trunk" | "dx" | "dioxus" => pick(sub, &[(&["serve", "watch"], Dev), (&["build", "bundle"], Build)]),
        "wasm-pack" => pick(sub, &[(&["build"], Build), (&["test"], Test)]),

        // ---------------- JVM
        "mvn" => scan(
            &pos,
            &[
                (
                    &[
                        "spring-boot:run",
                        "quarkus:dev",
                        "jetty:run",
                        "tomcat7:run",
                        "liberty:dev",
                        "micronaut:run",
                        "exec:java",
                    ],
                    Dev,
                ),
                (&["test", "verify", "integration-test"], Test),
                (&["package", "install", "compile", "deploy", "test-compile"], Build),
                (&["dependency:go-offline", "dependency:resolve", "dependency:copy-dependencies"], Install),
            ],
            false,
        ),
        // Gradle and Bazel run builds inside a long-lived server; that server is what we govern.
        "gradle-daemon" | "kotlin-daemon" | "a-server" => Some(Build),
        "sbt" => scan(
            &pos,
            &[
                (&["run", "~run", "reStart", "~reStart", "~compile", "~test"], Dev),
                (&["test", "testOnly", "it:test"], Test),
                (&["compile", "package", "assembly", "stage", "dist", "publishLocal"], Build),
                (&["update"], Install),
            ],
            false,
        ),
        "lein" => pick(
            sub,
            &[
                (&["run", "ring", "figwheel"], Dev),
                (&["test"], Test),
                (&["uberjar", "jar", "compile", "cljsbuild"], Build),
                (&["deps"], Install),
            ],
        ),

        // ---------------- PHP
        "php -S" => Some(Dev),
        "artisan" => {
            pick(sub, &[(&["serve", "octane:start", "octane:frankenphp", "reverb:start"], Dev), (&["test"], Test)])
        }
        "composer" => pick(
            sub,
            &[(&["install", "i", "update", "u", "upgrade", "require", "req", "reinstall", "create-project"], Install)],
        ),
        "phpunit" | "pest" | "paratest" => Some(Test),
        "codecept" => pick(sub, &[(&["run"], Test)]),
        "phpstan" | "psalm" | "phpcs" | "php-cs-fixer" | "rector" | "phan" => Some(Build),
        "symfony" => pick(sub, &[(&["serve", "server:start", "local:server:start"], Dev)]),
        "wp" => pick(sub, &[(&["server"], Dev)]),
        "frankenphp" => pick(sub, &[(&["run", "php-server"], Dev)]),

        // ---------------- Elixir / Erlang / Gleam
        "mix" => match sub {
            "phx.server" => Some(Dev),
            "run" if has("--no-halt") => Some(Dev),
            "test" => Some(Test),
            "compile" | "release" | "assets.deploy" | "assets.build" | "dialyzer" | "credo" | "escript.build"
            | "docs" => Some(Build),
            "deps.get" | "deps.compile" | "deps.update" | "local.hex" | "local.rebar" => Some(Install),
            _ => None,
        },
        "rebar3" => pick(
            sub,
            &[
                (&["compile", "release", "tar", "escriptize", "dialyzer", "xref"], Build),
                (&["ct", "eunit", "proper"], Test),
                (&["get-deps", "upgrade"], Install),
            ],
        ),
        "gleam" => pick(
            sub,
            &[(&["run"], Dev), (&["build", "check", "export"], Build), (&["test"], Test), (&["deps", "add"], Install)],
        ),

        // ---------------- .NET / Swift / Dart
        "dotnet" => pick(
            sub,
            &[
                (&["watch", "run"], Dev),
                (&["build", "publish", "pack", "msbuild", "format"], Build),
                (&["test", "vstest"], Test),
                (&["restore", "add", "tool"], Install),
            ],
        ),
        "swift" => match sub {
            "build" => Some(Build),
            "test" => Some(Test),
            "run" => Some(Dev),
            "package" if matches!(sub2, "resolve" | "update") => Some(Install),
            _ => None,
        },
        "xcodebuild" => Some(if has("test") || has("test-without-building") { Test } else { Build }),
        "flutter" => pick(
            sub,
            &[
                (&["run", "attach"], Dev),
                (&["build", "analyze"], Build),
                (&["test"], Test),
                (&["pub", "packages"], Install),
            ],
        ),
        "dart" => {
            pick(sub, &[(&["run"], Dev), (&["compile", "analyze"], Build), (&["test"], Test), (&["pub"], Install)])
        }

        // ---------------- native builds and task runners
        "make" => Some(
            scan(
                &pos,
                &[(&["dev", "serve", "server", "run", "start", "watch"], Dev), (&["test", "tests", "check"], Test)],
                false,
            )
            .unwrap_or(Build),
        ),
        "ninja" | "samu" | "cmake" | "meson" | "scons" | "xmake" | "msbuild" => {
            Some(if tool == "meson" && sub == "test" { Test } else { Build })
        }
        "ctest" => Some(Test),
        "just" | "task" | "mise" => scan(&pos, TASK_WORDS, false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(cmd: &str) -> Option<Kind> {
        classify(&cmd.split(' ').map(String::from).collect::<Vec<_>>(), &Custom::new())
    }

    #[test]
    fn browsers_driven_by_scripts_are_heavy_but_the_users_browser_is_not() {
        use Kind::*;
        let shell = "/Users/u/Library/Caches/ms-playwright/chromium_headless_shell-1223/chrome-headless-shell-mac-arm64/chrome-headless-shell";
        assert_eq!(c(&format!("{shell} --disable-field-trial-config --headless --no-sandbox")), Some(Test));
        assert_eq!(c("/usr/bin/chromium --headless=new --remote-debugging-port=0"), Some(Test));
        assert_eq!(c("/opt/firefox/firefox -headless -marionette"), Some(Test));
        let mac_chrome =
            vec!["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string(), "--headless".into()];
        assert_eq!(classify(&mac_chrome, &Custom::new()), Some(Test));
        assert_eq!(c("/usr/bin/chromium --profile-directory=Default"), None);
        assert_eq!(c("/Applications/Firefox.app/Contents/MacOS/firefox"), None);
    }

    #[test]
    fn javascript() {
        use Kind::*;
        assert_eq!(c("node /x/node_modules/next/dist/bin/next dev"), Some(Dev));
        assert_eq!(c("node /x/node_modules/.bin/next build --debug"), Some(Build));
        assert_eq!(c("node --max-old-space-size=8192 /x/next/dist/bin/next build"), Some(Build));
        assert_eq!(c("node -r /p.cjs --require /q.cjs /x/next/dist/bin/next start"), Some(Dev));
        assert_eq!(c("/usr/local/bin/node /usr/lib/node_modules/npm/bin/npm-cli.js ci"), Some(Install));
        assert_eq!(c("node /usr/lib/node_modules/npm/bin/npm-cli.js run build"), None);
        assert_eq!(c("node /usr/lib/node_modules/npm/bin/npx-cli.js -y next dev"), Some(Dev));
        assert_eq!(c("/Users/u/Library/pnpm/pnpm install --frozen-lockfile"), Some(Install));
        assert_eq!(c("node /x/.yarn/releases/yarn-4.5.0.cjs"), Some(Install));
        assert_eq!(c("node /x/.yarn/releases/yarn-4.5.0.cjs dev"), None);
        assert_eq!(c("node /x/typescript/bin/tsc --noEmit"), Some(Build));
        assert_eq!(c("node /x/typescript/bin/tsc -w"), Some(Dev));
        assert_eq!(c("node /x/vite/bin/vite.js"), Some(Dev));
        assert_eq!(c("node /x/vite/bin/vite.js build"), Some(Build));
        assert_eq!(c("node /x/vitest/vitest.mjs run"), Some(Test));
        assert_eq!(c("node /x/jest/bin/jest.js --watch"), Some(Dev));
        assert_eq!(c("node /x/@angular/cli/bin/ng.js serve"), Some(Dev));
        assert_eq!(c("node /x/turbo/bin/turbo run build --filter=web"), Some(Build));
        assert_eq!(c("/x/turbo run dev"), Some(Dev));
        assert_eq!(c("node /x/nx/bin/nx.js run web:serve"), Some(Dev));
        assert_eq!(classify(&["next-build (v16.4.5)".to_string()], &Custom::new()), Some(Build));
        assert_eq!(classify(&["next-server (v16.4.5)".to_string()], &Custom::new()), Some(Dev));
        assert_eq!(classify(&["npm ci".to_string()], &Custom::new()), Some(Install)); // npm's own title
        // Angular renames itself but keeps its arguments in the title
        let titled = |t: &str| classify(&[t.to_string(), String::new(), String::new()], &Custom::new());
        assert_eq!(titled("ng serve (my-app)"), Some(Dev));
        assert_eq!(c("node /x/.bin/react-router-serve ./build/server/index.js"), Some(Dev));
        assert_eq!(c("node /x/react-scripts/bin/react-scripts.js start"), Some(Dev));
        assert_eq!(c("node /x/.bin/react-scripts test --watchAll=false"), Some(Test));
        assert_eq!(c("node /x/.bin/expo start --clear"), Some(Dev));
        assert_eq!(c("node /x/.bin/react-native run-ios"), Some(Build));
        assert_eq!(c("node /x/.bin/storybook dev -p 6006"), Some(Dev));
        assert_eq!(c("node /x/.bin/tauri dev"), Some(Dev));
        assert_eq!(c("node /x/.bin/electron-forge start"), Some(Dev));
        assert_eq!(c("node /x/.bin/ember s"), Some(Dev));
        assert_eq!(c("node /x/.bin/vue-cli-service serve"), Some(Dev));
        assert_eq!(c("node /x/.bin/astro dev --port 4321"), Some(Dev));
        assert_eq!(c("node /x/.bin/nuxi dev"), Some(Dev));
        assert_eq!(c("node /x/.bin/gatsby develop"), Some(Dev));
        assert_eq!(c("node --test"), Some(Test));
        assert_eq!(c("node --watch server.js"), Some(Dev));
        assert_eq!(c("bun --hot server.ts"), Some(Dev));
        assert_eq!(c("bun install"), Some(Install));
        assert_eq!(c("bun run vite"), Some(Dev));
        assert_eq!(c("bun server.ts"), None);
        assert_eq!(c("deno run --watch main.ts"), Some(Dev));
        assert_eq!(c("deno task dev"), None);
        assert_eq!(c("node -e 0"), None);
        assert_eq!(c("node server.js"), None);
        assert_eq!(c("node /x/typescript/lib/tsserver.js"), None);
        assert_eq!(c("node"), None);
    }

    #[test]
    fn ruby() {
        use Kind::*;
        assert_eq!(c("ruby bin/rails server -p 3000"), Some(Dev));
        assert_eq!(c("/x/ruby /x/bin/bundle exec rails s"), Some(Dev));
        assert_eq!(c("ruby bin/rails test test/models"), Some(Test));
        assert_eq!(c("ruby bin/rails test:system"), Some(Test));
        assert_eq!(c("ruby bin/rails assets:precompile"), Some(Build));
        assert_eq!(c("ruby bin/rails tailwindcss:watch"), Some(Dev));
        assert_eq!(c("ruby bin/rails console"), None);
        assert_eq!(c("ruby bin/rails db:migrate"), None);
        assert_eq!(c("ruby /x/bin/bundle install"), Some(Install));
        assert_eq!(c("ruby /x/bin/rspec spec/models"), Some(Test));
        assert_eq!(c("ruby /x/bin/foreman start -f Procfile.dev"), Some(Dev));
        assert_eq!(c("ruby -S rake test"), Some(Test));
        // puma rewrites its title; KERN_PROCARGS2 then shows one argv entry with spaces
        let titled = vec!["puma 8.0.3 (tcp://[::]:4791) [rack]      ".to_string(), String::new()];
        assert_eq!(classify(&titled, &Custom::new()), Some(Dev));
        let sidekiq = vec!["sidekiq 7.3.0 app [0 of 5 busy]".to_string()];
        assert_eq!(classify(&sidekiq, &Custom::new()), None);
    }

    #[test]
    fn python() {
        use Kind::*;
        assert_eq!(c("/x/python3.12 manage.py runserver 0.0.0.0:8000"), Some(Dev));
        assert_eq!(c("python manage.py test"), Some(Test));
        assert_eq!(c("python manage.py migrate"), None);
        assert_eq!(c("/x/.venv/bin/python /x/.venv/bin/uvicorn app:app --reload"), Some(Dev));
        assert_eq!(c("python -m uvicorn app:app"), Some(Dev));
        assert_eq!(c("python3 -m http.server 8000"), Some(Dev));
        assert_eq!(c("/x/python /x/bin/flask --app hello run --debug"), Some(Dev));
        assert_eq!(c("gunicorn: master [app:app]"), Some(Dev));
        assert_eq!(c("python -m pytest -n auto"), Some(Test));
        assert_eq!(c("/x/python /x/bin/py.test"), Some(Test));
        assert_eq!(c("python -m pip install -r requirements.txt"), Some(Install));
        assert_eq!(c("/x/uv sync"), Some(Install));
        assert_eq!(c("/x/uv run pytest"), Some(Test));
        assert_eq!(c("/x/uv run python manage.py runserver"), Some(Dev));
        assert_eq!(c("python /x/bin/poetry install"), Some(Install));
        assert_eq!(c("python -c print(1)"), None);
        assert_eq!(c("python /x/bin/celery -A proj worker"), None);
        assert_eq!(c("python3 /x/jupyter-lab"), None);
    }

    #[test]
    fn go_rust_jvm_php_elixir_dotnet_native() {
        use Kind::*;
        assert_eq!(c("go run ./cmd/server"), Some(Dev));
        assert_eq!(c("go test ./..."), Some(Test));
        assert_eq!(c("go build -o bin/app ."), Some(Build));
        assert_eq!(c("go mod download"), Some(Install));
        assert_eq!(c("/x/air -c .air.toml"), Some(Dev));
        assert_eq!(c("hugo server -D"), Some(Dev));
        assert_eq!(c("hugo"), Some(Build));
        assert_eq!(c("cargo build --release"), Some(Build));
        assert_eq!(c("cargo test"), Some(Test));
        assert_eq!(c("cargo run --bin api"), Some(Dev));
        assert_eq!(c("cargo leptos watch"), Some(Dev));
        assert_eq!(c("cargo fmt"), None);
        let mvn = "java -classpath /m/boot/plexus-classworlds-2.8.0.jar -Dclassworlds.conf=/m/bin/m2.conf -Dmaven.home=/m org.codehaus.plexus.classworlds.launcher.Launcher";
        assert_eq!(c(&format!("{mvn} spring-boot:run")), Some(Dev));
        assert_eq!(c(&format!("{mvn} -P dev clean verify")), Some(Test));
        assert_eq!(c(&format!("{mvn} -T 4 clean install")), Some(Build));
        assert_eq!(
            c(
                "java --add-opens java.base/java.util=ALL-UNNAMED -Xmx2g -cp /g/gradle-daemon-main.jar org.gradle.launcher.daemon.bootstrap.GradleDaemon 8.10"
            ),
            Some(Build)
        );
        assert_eq!(c("java -cp /g/gradle-wrapper.jar org.gradle.wrapper.GradleWrapperMain build"), None);
        assert_eq!(c("java -jar /x/sbt-launch.jar ~run"), Some(Dev));
        assert_eq!(c("java -jar app.jar"), None);
        assert_eq!(c("java -cp app.jar com.example.Application"), None);
        assert_eq!(c("php artisan serve"), Some(Dev));
        assert_eq!(c("php -S localhost:8000 -t public"), Some(Dev));
        assert_eq!(c("php /x/composer.phar install"), Some(Install));
        assert_eq!(c("php vendor/bin/phpunit"), Some(Test));
        assert_eq!(c("php artisan queue:work"), None);
        assert_eq!(c("/x/erts-15/bin/beam.smp -- -root /x -- -extra /usr/local/bin/mix phx.server"), Some(Dev));
        assert_eq!(c("/x/beam.smp -- -root /x -- -extra /usr/local/bin/mix test"), Some(Test));
        assert_eq!(c("dotnet watch run"), Some(Dev));
        assert_eq!(c("dotnet test"), Some(Test));
        assert_eq!(c("dotnet /x/sdk/MSBuild.dll /nodemode:1"), None);
        assert_eq!(c("swift build -c release"), Some(Build));
        assert_eq!(c("make -j8"), Some(Build));
        assert_eq!(c("make dev"), Some(Dev));
        assert_eq!(c("make test"), Some(Test));
        assert_eq!(c("dart --packages=/x /x/flutter_tools.snapshot run -d chrome"), Some(Dev));
        assert_eq!(c("just dev"), Some(Dev));
        assert_eq!(c("just fmt"), None);
        assert_eq!(c("docker compose up"), None);
        assert_eq!(c("postgres -D /x"), None);
    }

    #[test]
    fn renamed_tools_get_their_arguments_back_from_the_package_script() {
        use Kind::*;
        // webpack-cli sets its title to a bare "webpack": what nodoka reads on macOS / Linux
        let mac = || vec!["webpack".to_string(), String::new(), String::new(), String::new()];
        let k = |argv: Vec<String>, script: Option<&str>| classify(&unwipe(argv, script), &Custom::new());
        assert_eq!(k(mac(), None), Some(Build), "nothing to go on: a bare `webpack` is a build");
        assert_eq!(k(mac(), Some("webpack serve --mode development")), Some(Dev));
        assert_eq!(k(vec!["webpack".into()], Some("cross-env NODE_ENV=dev webpack --watch && echo")), Some(Dev));
        assert_eq!(k(mac(), Some("tsc -b && webpack --mode production")), Some(Build));
        // a tool that wasn't renamed keeps its own arguments, whatever script started it
        let real = "node /x/.bin/vite build".split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(unwipe(real.clone(), Some("vite")), real);
        // titles with words (npm, Next, puma) are left to `untitle`
        let npm = vec!["npm ci".to_string(), String::new()];
        assert_eq!(unwipe(npm.clone(), Some("npm ci && npm run build")), npm);
    }

    #[test]
    fn custom_rules_win() {
        let mut custom = Custom::new();
        custom.insert("my-server".into(), Some(Kind::Dev));
        custom.insert("next".into(), None);
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(classify(&v("/x/my-server --port 9000"), &custom), Some(Kind::Dev));
        assert_eq!(classify(&v("node /x/next/dist/bin/next build"), &custom), None);
    }

    #[test]
    fn stems() {
        assert_eq!(stem("/opt/homebrew/bin/python3.12"), "python");
        assert_eq!(stem("Python"), "python");
        assert_eq!(stem("ruby-lsp"), "ruby-lsp");
        assert_eq!(stem("/x/rubocop"), "rubocop");
        assert_eq!(stem("vite.js"), "vite");
        assert_eq!(stem("gunicorn:"), "gunicorn");
        assert_eq!(stem("/x/erts-15/bin/beam.smp"), "beam");
    }
}
