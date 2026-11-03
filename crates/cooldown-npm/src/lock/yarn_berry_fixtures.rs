//! Real Yarn Berry locks the Berry readers' tests share, one per lockfile schema.
//!
//! Each lock was written by `yarn install --mode=update-lockfile` against registry.yarnpkg.com
//! in a scratch directory holding exactly the files listed beside it and a `.yarnrc.yml` of
//! `enableTelemetry: false` and `nodeLinker: node-modules`.
//! Yarn ran as `node .../yarn.js` from mise's `yarn@<version>` install, under Node 22.23.1 for
//! Yarn 2 and 3, whose bundles call APIs Node 23 removed, and Node 26.3.1 for Yarn 4.
//! Yarn 2.4.3 has no `--mode` flag, so it ran a plain `yarn install`.
//! Every line, the checksums included, is Yarn's own output, and each manifest is as Yarn left it
//! after normalizing it.

use indoc::indoc;

/// One file of a fixture project.
pub(crate) struct ProjectFile {
    /// The project-relative, `/`-separated path.
    pub(crate) path: &'static str,
    pub(crate) contents: &'static str,
}

/// A project as its fixture records it: the files Yarn read and the lock it wrote.
pub(crate) struct BerryProject {
    /// Every manifest and input file, the root manifest first.
    pub(crate) files: &'static [ProjectFile],
    /// The `yarn.lock` Yarn wrote for them.
    pub(crate) lock: &'static str,
}

/// A Yarn 4.18.1 workspace (lockfile schema 10) resolving every kind of locator a Berry lock
/// records.
///
/// The root declares registry ranges (`chalk`, `is-number`, the scoped `@sindresorhus/is`), a
/// dist-tag (`is-odd@latest`), an npm alias (`isnum` of `is-number@6.0.0`, which `is-odd`'s own
/// `^6.0.0` also resolves to), `resolve`, which Yarn wraps in its builtin patch, an optional
/// platform package Yarn records with `conditions` and without a checksum, and a `link:`,
/// `portal:`, `file:`, `exec:`, git, and tarball URL dependency each.
/// The portal and the file package depend on registry packages of their own.
/// `pkg-a` pins `chalk@4.1.2`, which groups with the root's caret, and depends on `pkg-b` through
/// `workspace:^`; `pkg-b` pins `chalk@4.1.0`.
/// Its `.yarnrc.yml` also sets `enableScripts: true` and `approvedGitRepositories: ["**"]`, which
/// the `exec:` and git dependencies need.
pub(crate) const EVERY_SOURCE: BerryProject = BerryProject {
    files: &[
        ProjectFile {
            path: "package.json",
            contents: indoc! {r#"
                {
                  "name": "berry-root",
                  "private": true,
                  "packageManager": "yarn@4.18.1",
                  "workspaces": [
                    "packages/*"
                  ],
                  "dependencies": {
                    "@sindresorhus/is": "^4.0.0",
                    "chalk": "^4.1.0",
                    "gen": "exec:./gen.js",
                    "is-number": "^7.0.0",
                    "is-odd": "latest",
                    "isnum": "npm:is-number@6.0.0",
                    "left-pad": "https://github.com/stevemao/left-pad.git#v1.3.0",
                    "local-file": "file:./vendor/file-target",
                    "local-link": "link:./vendor/link-target",
                    "local-portal": "portal:./vendor/portal-target",
                    "ms-tarball": "https://registry.npmjs.org/ms/-/ms-2.1.3.tgz",
                    "resolve": "^1.22.0"
                  },
                  "optionalDependencies": {
                    "@esbuild/darwin-arm64": "0.25.0"
                  }
                }
            "#},
        },
        ProjectFile {
            path: "packages/a/package.json",
            contents: indoc! {r#"
                {
                  "name": "pkg-a",
                  "version": "1.0.0",
                  "dependencies": {
                    "chalk": "4.1.2",
                    "pkg-b": "workspace:^"
                  }
                }
            "#},
        },
        ProjectFile {
            path: "packages/b/package.json",
            contents: indoc! {r#"
                {
                  "name": "pkg-b",
                  "version": "1.0.0",
                  "dependencies": {
                    "chalk": "4.1.0"
                  }
                }
            "#},
        },
        ProjectFile {
            path: "vendor/link-target/package.json",
            contents: indoc! {r#"
                { "name": "local-link", "version": "1.0.0" }
            "#},
        },
        ProjectFile {
            path: "vendor/portal-target/package.json",
            contents: indoc! {r#"
                { "name": "local-portal", "version": "1.0.0", "dependencies": { "is-number": "^7.0.0" } }
            "#},
        },
        ProjectFile {
            path: "vendor/file-target/package.json",
            contents: indoc! {r#"
                { "name": "local-file", "version": "1.0.0", "dependencies": { "is-odd": "^3.0.1" } }
            "#},
        },
        ProjectFile {
            path: "gen.js",
            contents: indoc! {r"
                const fs = require(`fs`);
                const path = require(`path`);
                fs.writeFileSync(path.join(execEnv.buildDir, `package.json`), JSON.stringify({name: `gen`, version: `1.0.0`}));
            "},
        },
    ],
    lock: indoc! {r#"
        # This file is generated by running "yarn install" inside your project.
        # Manual changes might be lost - proceed with caution!

        __metadata:
          version: 10
          cacheKey: 10c0

        "@esbuild/darwin-arm64@npm:0.25.0":
          version: 0.25.0
          resolution: "@esbuild/darwin-arm64@npm:0.25.0"
          conditions: os=darwin & cpu=arm64
          languageName: node
          linkType: hard

        "@sindresorhus/is@npm:^4.0.0":
          version: 4.6.0
          resolution: "@sindresorhus/is@npm:4.6.0"
          checksum: 10c0/33b6fb1d0834ec8dd7689ddc0e2781c2bfd8b9c4e4bacbcb14111e0ae00621f2c264b8a7d36541799d74888b5dccdf422a891a5cb5a709ace26325eedc81e22e
          languageName: node
          linkType: hard

        "ansi-styles@npm:^4.1.0":
          version: 4.3.0
          resolution: "ansi-styles@npm:4.3.0"
          dependencies:
            color-convert: "npm:^2.0.1"
          checksum: 10c0/895a23929da416f2bd3de7e9cb4eabd340949328ab85ddd6e484a637d8f6820d485f53933446f5291c3b760cbc488beb8e88573dd0f9c7daf83dccc8fe81b041
          languageName: node
          linkType: hard

        "berry-root@workspace:.":
          version: 0.0.0-use.local
          resolution: "berry-root@workspace:."
          dependencies:
            "@esbuild/darwin-arm64": "npm:0.25.0"
            "@sindresorhus/is": "npm:^4.0.0"
            chalk: "npm:^4.1.0"
            gen: "exec:./gen.js"
            is-number: "npm:^7.0.0"
            is-odd: "npm:latest"
            isnum: "npm:is-number@6.0.0"
            left-pad: "https://github.com/stevemao/left-pad.git#v1.3.0"
            local-file: "file:./vendor/file-target"
            local-link: "link:./vendor/link-target"
            local-portal: "portal:./vendor/portal-target"
            ms-tarball: "https://registry.npmjs.org/ms/-/ms-2.1.3.tgz"
            resolve: "npm:^1.22.0"
          dependenciesMeta:
            "@esbuild/darwin-arm64":
              optional: true
          languageName: unknown
          linkType: soft

        "chalk@npm:4.1.0":
          version: 4.1.0
          resolution: "chalk@npm:4.1.0"
          dependencies:
            ansi-styles: "npm:^4.1.0"
            supports-color: "npm:^7.1.0"
          checksum: 10c0/3787bd65ecd98ab3a1acc3b4f71d006268a675875e49ee6ea75fb54ba73d268b97544368358c18c42445e408e076ae8ad5cec8fbad36942a2c7ac654883dc61e
          languageName: node
          linkType: hard

        "chalk@npm:4.1.2, chalk@npm:^4.1.0":
          version: 4.1.2
          resolution: "chalk@npm:4.1.2"
          dependencies:
            ansi-styles: "npm:^4.1.0"
            supports-color: "npm:^7.1.0"
          checksum: 10c0/4a3fef5cc34975c898ffe77141450f679721df9dde00f6c304353fa9c8b571929123b26a0e4617bde5018977eb655b31970c297b91b63ee83bb82aeb04666880
          languageName: node
          linkType: hard

        "color-convert@npm:^2.0.1":
          version: 2.0.1
          resolution: "color-convert@npm:2.0.1"
          dependencies:
            color-name: "npm:~1.1.4"
          checksum: 10c0/37e1150172f2e311fe1b2df62c6293a342ee7380da7b9cfdba67ea539909afbd74da27033208d01d6d5cfc65ee7868a22e18d7e7648e004425441c0f8a15a7d7
          languageName: node
          linkType: hard

        "color-name@npm:~1.1.4":
          version: 1.1.4
          resolution: "color-name@npm:1.1.4"
          checksum: 10c0/a1a3f914156960902f46f7f56bc62effc6c94e84b2cae157a526b1c1f74b677a47ec602bf68a61abfa2b42d15b7c5651c6dbe72a43af720bc588dff885b10f95
          languageName: node
          linkType: hard

        "es-errors@npm:^1.3.0":
          version: 1.3.0
          resolution: "es-errors@npm:1.3.0"
          checksum: 10c0/0a61325670072f98d8ae3b914edab3559b6caa980f08054a3b872052640d91da01d38df55df797fcc916389d77fc92b8d5906cf028f4db46d7e3003abecbca85
          languageName: node
          linkType: hard

        "function-bind@npm:^1.1.2":
          version: 1.1.2
          resolution: "function-bind@npm:1.1.2"
          checksum: 10c0/d8680ee1e5fcd4c197e4ac33b2b4dce03c71f4d91717292785703db200f5c21f977c568d28061226f9b5900cbcd2c84463646134fd5337e7925e0942bc3f46d5
          languageName: node
          linkType: hard

        "gen@exec:./gen.js::locator=berry-root%40workspace%3A.":
          version: 1.0.0
          resolution: "gen@exec:./gen.js#./gen.js::hash=ac36e6&locator=berry-root%40workspace%3A."
          checksum: 10c0/1c200de61986ff03f220e2fb6669e4e099242b90453b6705bfca58cc6c13d39fc29597ea0f2e47344dde032c4e06b9157b7c8335b8805e86134a40d5f5580d17
          languageName: node
          linkType: hard

        "has-flag@npm:^4.0.0":
          version: 4.0.0
          resolution: "has-flag@npm:4.0.0"
          checksum: 10c0/2e789c61b7888d66993e14e8331449e525ef42aac53c627cc53d1c3334e768bcb6abdc4f5f0de1478a25beec6f0bd62c7549058b7ac53e924040d4f301f02fd1
          languageName: node
          linkType: hard

        "hasown@npm:^2.0.4":
          version: 2.0.4
          resolution: "hasown@npm:2.0.4"
          dependencies:
            function-bind: "npm:^1.1.2"
          checksum: 10c0/2d8de939e270b70618f8cebb69746620db10617dbb495bc66ddad326955ea24d3ca4af133aff3eb7c1853e0218f867bc2b050ec26fe02e3aea58f880ffc5e506
          languageName: node
          linkType: hard

        "is-core-module@npm:^2.17.0":
          version: 2.17.0
          resolution: "is-core-module@npm:2.17.0"
          dependencies:
            hasown: "npm:^2.0.4"
          checksum: 10c0/3dc7c15ea42f846180cd2cdd57af62270e0a8d0ed31c17c85735d554089d39975e2e82e65fc7c8280a3c476ca3d0d09992d0860b4998abb769a277341feb6698
          languageName: node
          linkType: hard

        "is-number@npm:^6.0.0, isnum@npm:is-number@6.0.0":
          version: 6.0.0
          resolution: "is-number@npm:6.0.0"
          checksum: 10c0/5da4c68401529675c575878d2760d66f18eaef4b014858577f6003daf66488d7fe4eae684b1e8574e3fa1bb447c6c6c56b8491d2b4b3239da2d32e5f6f218008
          languageName: node
          linkType: hard

        "is-number@npm:^7.0.0":
          version: 7.0.0
          resolution: "is-number@npm:7.0.0"
          checksum: 10c0/b4686d0d3053146095ccd45346461bc8e53b80aeb7671cc52a4de02dbbf7dc0d1d2a986e2fe4ae206984b4d34ef37e8b795ebc4f4295c978373e6575e295d811
          languageName: node
          linkType: hard

        "is-odd@npm:^3.0.1, is-odd@npm:latest":
          version: 3.0.1
          resolution: "is-odd@npm:3.0.1"
          dependencies:
            is-number: "npm:^6.0.0"
          checksum: 10c0/89ee2e353c5a3f3bd400c79db1c307a5b3506198ee8169d521e533a9b1d8a08fc95f21a919c084e98845b4286d7ffe309778da03744dfe66c3c1763ab1a030c6
          languageName: node
          linkType: hard

        "left-pad@https://github.com/stevemao/left-pad.git#v1.3.0":
          version: 1.3.0
          resolution: "left-pad@https://github.com/stevemao/left-pad.git#commit=eb115f2f0bee68ee3534eac37f50218778ca4507"
          checksum: 10c0/4bbf696341247411a53fa50755bd2444ed1355ec5a110254eaffa81a5538fb9c9c0f02a7271b330a79af3b46968776bbd77c44245171e7384000f44c85140a41
          languageName: node
          linkType: hard

        "local-file@file:./vendor/file-target::locator=berry-root%40workspace%3A.":
          version: 1.0.0
          resolution: "local-file@file:./vendor/file-target#./vendor/file-target::hash=480243&locator=berry-root%40workspace%3A."
          dependencies:
            is-odd: "npm:^3.0.1"
          checksum: 10c0/5b31bb9d1a4f8e7cf606840e9599f1c109a717c6f0ba1d540416a95c3a57ad14c3218f0f6f95738985dc8eeb521e2d9bdd3b4ebc41986db87534f6de2765ee07
          languageName: node
          linkType: hard

        "local-link@link:./vendor/link-target::locator=berry-root%40workspace%3A.":
          version: 0.0.0-use.local
          resolution: "local-link@link:./vendor/link-target::locator=berry-root%40workspace%3A."
          languageName: node
          linkType: soft

        "local-portal@portal:./vendor/portal-target::locator=berry-root%40workspace%3A.":
          version: 0.0.0-use.local
          resolution: "local-portal@portal:./vendor/portal-target::locator=berry-root%40workspace%3A."
          dependencies:
            is-number: "npm:^7.0.0"
          languageName: node
          linkType: soft

        "ms-tarball@https://registry.npmjs.org/ms/-/ms-2.1.3.tgz":
          version: 2.1.3
          resolution: "ms-tarball@https://registry.npmjs.org/ms/-/ms-2.1.3.tgz"
          checksum: 10c0/e150e83f8b35d71cc99f42e75fd93a07849e037aa39fa0ad392d5520731614508fd348f4a412563d1eafedbcbe53d78ede70f75a27e330e4a4addb2ffdd11b9a
          languageName: node
          linkType: hard

        "path-parse@npm:^1.0.7":
          version: 1.0.7
          resolution: "path-parse@npm:1.0.7"
          checksum: 10c0/11ce261f9d294cc7a58d6a574b7f1b935842355ec66fba3c3fd79e0f036462eaf07d0aa95bb74ff432f9afef97ce1926c720988c6a7451d8a584930ae7de86e1
          languageName: node
          linkType: hard

        "pkg-a@workspace:packages/a":
          version: 0.0.0-use.local
          resolution: "pkg-a@workspace:packages/a"
          dependencies:
            chalk: "npm:4.1.2"
            pkg-b: "workspace:^"
          languageName: unknown
          linkType: soft

        "pkg-b@workspace:^, pkg-b@workspace:packages/b":
          version: 0.0.0-use.local
          resolution: "pkg-b@workspace:packages/b"
          dependencies:
            chalk: "npm:4.1.0"
          languageName: unknown
          linkType: soft

        "resolve@npm:^1.22.0":
          version: 1.22.13
          resolution: "resolve@npm:1.22.13"
          dependencies:
            es-errors: "npm:^1.3.0"
            is-core-module: "npm:^2.17.0"
            path-parse: "npm:^1.0.7"
            supports-preserve-symlinks-flag: "npm:^1.0.0"
          bin:
            resolve: bin/resolve
          checksum: 10c0/8dac5b33a1f9f241500fcaf42f9033cdcaf3738c908a60a50a976ff2f6129063cbc5368b4092e42d2d264ad22aa94aaff207f3efa8d0185c68454439b11a1973
          languageName: node
          linkType: hard

        "resolve@patch:resolve@npm%3A^1.22.0#optional!builtin<compat/resolve>":
          version: 1.22.13
          resolution: "resolve@patch:resolve@npm%3A1.22.13#optional!builtin<compat/resolve>::version=1.22.13&hash=9bd1a5"
          dependencies:
            es-errors: "npm:^1.3.0"
            is-core-module: "npm:^2.17.0"
            path-parse: "npm:^1.0.7"
            supports-preserve-symlinks-flag: "npm:^1.0.0"
          bin:
            resolve: bin/resolve
          checksum: 10c0/bc58ee02e647a4250453eeb067796bdc087f6a89d4d5780bfe479f402ec11b9461babc155a6452020b56f3845cf49b1afd9c95fe3fc8347cba6567aa9b10c106
          languageName: node
          linkType: hard

        "supports-color@npm:^7.1.0":
          version: 7.2.0
          resolution: "supports-color@npm:7.2.0"
          dependencies:
            has-flag: "npm:^4.0.0"
          checksum: 10c0/afb4c88521b8b136b5f5f95160c98dee7243dc79d5432db7efc27efb219385bbc7d9427398e43dd6cc730a0f87d5085ce1652af7efbe391327bc0a7d0f7fc124
          languageName: node
          linkType: hard

        "supports-preserve-symlinks-flag@npm:^1.0.0":
          version: 1.0.0
          resolution: "supports-preserve-symlinks-flag@npm:1.0.0"
          checksum: 10c0/6c4032340701a9950865f7ae8ef38578d8d7053f5e10518076e6554a9381fa91bd9c6850193695c141f32b21f979c985db07265a758867bac95de05f7d8aeb39
          languageName: node
          linkType: hard
    "#},
};

/// A Yarn 4.18.1 project (lockfile schema 10) whose root `resolutions` rewrite `chalk`'s
/// `supports-color@npm:^7.1.0` to `7.1.0`, so that dependency's descriptor is no key of the lock.
pub(crate) const RESOLUTIONS: BerryProject = BerryProject {
    files: &[ProjectFile {
        path: "package.json",
        contents: indoc! {r#"
                {
                  "name": "berry-resolutions",
                  "private": true,
                  "packageManager": "yarn@4.18.1",
                  "dependencies": {
                    "chalk": "^4.1.0"
                  },
                  "resolutions": {
                    "supports-color": "7.1.0"
                  }
                }
            "#},
    }],
    lock: indoc! {r#"
        # This file is generated by running "yarn install" inside your project.
        # Manual changes might be lost - proceed with caution!

        __metadata:
          version: 10
          cacheKey: 10c0

        "ansi-styles@npm:^4.1.0":
          version: 4.3.0
          resolution: "ansi-styles@npm:4.3.0"
          dependencies:
            color-convert: "npm:^2.0.1"
          checksum: 10c0/895a23929da416f2bd3de7e9cb4eabd340949328ab85ddd6e484a637d8f6820d485f53933446f5291c3b760cbc488beb8e88573dd0f9c7daf83dccc8fe81b041
          languageName: node
          linkType: hard

        "berry-resolutions@workspace:.":
          version: 0.0.0-use.local
          resolution: "berry-resolutions@workspace:."
          dependencies:
            chalk: "npm:^4.1.0"
          languageName: unknown
          linkType: soft

        "chalk@npm:^4.1.0":
          version: 4.1.2
          resolution: "chalk@npm:4.1.2"
          dependencies:
            ansi-styles: "npm:^4.1.0"
            supports-color: "npm:^7.1.0"
          checksum: 10c0/4a3fef5cc34975c898ffe77141450f679721df9dde00f6c304353fa9c8b571929123b26a0e4617bde5018977eb655b31970c297b91b63ee83bb82aeb04666880
          languageName: node
          linkType: hard

        "color-convert@npm:^2.0.1":
          version: 2.0.1
          resolution: "color-convert@npm:2.0.1"
          dependencies:
            color-name: "npm:~1.1.4"
          checksum: 10c0/37e1150172f2e311fe1b2df62c6293a342ee7380da7b9cfdba67ea539909afbd74da27033208d01d6d5cfc65ee7868a22e18d7e7648e004425441c0f8a15a7d7
          languageName: node
          linkType: hard

        "color-name@npm:~1.1.4":
          version: 1.1.4
          resolution: "color-name@npm:1.1.4"
          checksum: 10c0/a1a3f914156960902f46f7f56bc62effc6c94e84b2cae157a526b1c1f74b677a47ec602bf68a61abfa2b42d15b7c5651c6dbe72a43af720bc588dff885b10f95
          languageName: node
          linkType: hard

        "has-flag@npm:^4.0.0":
          version: 4.0.0
          resolution: "has-flag@npm:4.0.0"
          checksum: 10c0/2e789c61b7888d66993e14e8331449e525ef42aac53c627cc53d1c3334e768bcb6abdc4f5f0de1478a25beec6f0bd62c7549058b7ac53e924040d4f301f02fd1
          languageName: node
          linkType: hard

        "supports-color@npm:7.1.0":
          version: 7.1.0
          resolution: "supports-color@npm:7.1.0"
          dependencies:
            has-flag: "npm:^4.0.0"
          checksum: 10c0/e2e6d0c4a0165790801c87b6b16f3e1e0751b4e26e0fdd3c3ff41dd00f6f933548ac3f06b6b9225c27117c386e270f77f08350e50bdef5070cd67d13893bbb66
          languageName: node
          linkType: hard
    "#},
};

/// A Yarn 4.18.1 workspace (lockfile schema 10) whose member directories are named `100%`, `a#b`,
/// and `c::d`, which Yarn records verbatim in each workspace's locator.
/// The root depends on `hash` through a `workspace:` range naming its directory, so a descriptor
/// carries such a path too.
pub(crate) const LITERAL_PATHS: BerryProject = BerryProject {
    files: &[
        ProjectFile {
            path: "package.json",
            contents: indoc! {r#"
                {
                  "name": "berry-paths",
                  "private": true,
                  "packageManager": "yarn@4.18.1",
                  "workspaces": [
                    "packages/*"
                  ],
                  "dependencies": {
                    "hash": "workspace:packages/a#b",
                    "percent": "workspace:*"
                  }
                }
            "#},
        },
        ProjectFile {
            path: "packages/100%/package.json",
            contents: indoc! {r#"
                {
                  "name": "percent",
                  "version": "1.0.0",
                  "dependencies": {
                    "is-number": "^7.0.0"
                  }
                }
            "#},
        },
        ProjectFile {
            path: "packages/a#b/package.json",
            contents: indoc! {r#"
                {
                  "name": "hash",
                  "version": "1.0.0"
                }
            "#},
        },
        ProjectFile {
            path: "packages/c::d/package.json",
            contents: indoc! {r#"
                {
                  "name": "colons",
                  "version": "1.0.0"
                }
            "#},
        },
    ],
    lock: indoc! {r#"
        # This file is generated by running "yarn install" inside your project.
        # Manual changes might be lost - proceed with caution!

        __metadata:
          version: 10
          cacheKey: 10c0

        "berry-paths@workspace:.":
          version: 0.0.0-use.local
          resolution: "berry-paths@workspace:."
          dependencies:
            hash: "workspace:packages/a#b"
            percent: "workspace:*"
          languageName: unknown
          linkType: soft

        "colons@workspace:packages/c::d":
          version: 0.0.0-use.local
          resolution: "colons@workspace:packages/c::d"
          languageName: unknown
          linkType: soft

        "hash@workspace:packages/a#b":
          version: 0.0.0-use.local
          resolution: "hash@workspace:packages/a#b"
          languageName: unknown
          linkType: soft

        "is-number@npm:^7.0.0":
          version: 7.0.0
          resolution: "is-number@npm:7.0.0"
          checksum: 10c0/b4686d0d3053146095ccd45346461bc8e53b80aeb7671cc52a4de02dbbf7dc0d1d2a986e2fe4ae206984b4d34ef37e8b795ebc4f4295c978373e6575e295d811
          languageName: node
          linkType: hard

        "percent@workspace:*, percent@workspace:packages/100%":
          version: 0.0.0-use.local
          resolution: "percent@workspace:packages/100%"
          dependencies:
            is-number: "npm:^7.0.0"
          languageName: unknown
          linkType: soft
    "#},
};

/// The workspace each [`SCHEMAS`] lock was written for, which declares a registry range, a scoped
/// package, an npm alias, `resolve` (wrapped in Yarn's builtin patch), and a member pinning each
/// copy of `chalk`, with `pkg-b` linked through `workspace:*`, which every Berry release reads.
///
/// Each lock's root manifest names the Yarn that wrote it in `packageManager`, as this one names
/// Yarn 4.0.2.
pub(crate) const SCHEMA_FILES: &[ProjectFile] = &[
    ProjectFile {
        path: "package.json",
        contents: indoc! {r#"
            {
              "name": "berry-root",
              "private": true,
              "packageManager": "yarn@4.0.2",
              "workspaces": [
                "packages/*"
              ],
              "dependencies": {
                "@sindresorhus/is": "^4.0.0",
                "chalk": "^4.1.0",
                "is-number": "^7.0.0",
                "isnum": "npm:is-number@6.0.0",
                "resolve": "^1.22.0"
              }
            }
        "#},
    },
    ProjectFile {
        path: "packages/a/package.json",
        contents: indoc! {r#"
            {
              "name": "pkg-a",
              "version": "1.0.0",
              "dependencies": {
                "chalk": "4.1.2",
                "pkg-b": "workspace:*"
              }
            }
        "#},
    },
    ProjectFile {
        path: "packages/b/package.json",
        contents: indoc! {r#"
            {
              "name": "pkg-b",
              "version": "1.0.0",
              "dependencies": {
                "chalk": "4.1.0"
              }
            }
        "#},
    },
];

/// One lock of [`SCHEMAS`]: the Yarn release that wrote it and its schema.
pub(crate) struct SchemaLock {
    /// The Yarn release.
    pub(crate) yarn: &'static str,
    /// The lock's `__metadata.version`.
    pub(crate) schema: u64,
    pub(crate) lock: &'static str,
}

/// The [`SCHEMA_FILES`] workspace as each lockfile schema family records it.
///
/// Schema 4 (Yarn 2.4.3) keys `resolve@^1.22.0` without its `npm:` protocol.
/// Schemas up to 6 wrap a protocol-less inner range in `resolve`'s builtin patch and omit the
/// protocol from every dependency.
/// Schema 8 (Yarn 4.0.2) records checksums without their cache key, and schemas 9 and 10 differ
/// from 8 in the builtin patch's hash alone.
pub(crate) const SCHEMAS: &[SchemaLock] = &[
    SchemaLock {
        yarn: "2.4.3",
        schema: 4,
        lock: indoc! {r#"
            # This file is generated by running "yarn install" inside your project.
            # Manual changes might be lost - proceed with caution!

            __metadata:
              version: 4
              cacheKey: 7

            "@sindresorhus/is@npm:^4.0.0":
              version: 4.6.0
              resolution: "@sindresorhus/is@npm:4.6.0"
              checksum: 9a7a8cacfcb49534a1416d2d0a3c6ae6e892bd6e22544a49334a5f846437498b80fbe3dae2d2bd454f0b25e3acc9db3b202b2bbe69052ef1e6d2b26d6d386cb8
              languageName: node
              linkType: hard

            "ansi-styles@npm:^4.1.0":
              version: 4.3.0
              resolution: "ansi-styles@npm:4.3.0"
              dependencies:
                color-convert: ^2.0.1
              checksum: ea02c0179f3dd089a161f5fdd7ccd89dd84f31d82b68869f1134bf5c5b9e1313dadd2ff9edb02b44f46243f285ef5b785f6cb61c84a293694221417c42934407
              languageName: node
              linkType: hard

            "berry-root@workspace:.":
              version: 0.0.0-use.local
              resolution: "berry-root@workspace:."
              dependencies:
                "@sindresorhus/is": ^4.0.0
                chalk: ^4.1.0
                is-number: ^7.0.0
                isnum: "npm:is-number@6.0.0"
                resolve: ^1.22.0
              languageName: unknown
              linkType: soft

            "chalk@npm:4.1.0":
              version: 4.1.0
              resolution: "chalk@npm:4.1.0"
              dependencies:
                ansi-styles: ^4.1.0
                supports-color: ^7.1.0
              checksum: f860285b419f9e925c2db0f45ffa88aa8794c14b80cc5d01ff30930bcfc384996606362706f0829cf557f6d36152a5fb2d227ad63c4bc90e2ec9e9dbed4a3c07
              languageName: node
              linkType: hard

            "chalk@npm:4.1.2, chalk@npm:^4.1.0":
              version: 4.1.2
              resolution: "chalk@npm:4.1.2"
              dependencies:
                ansi-styles: ^4.1.0
                supports-color: ^7.1.0
              checksum: e3901b97d953991712bf0b941d586175be7ca5da56a97d25187e07453c6b26cae0ac8d9c7aa9e87e7c5c986fff870771b3a8e2705b3becda868829e2e12c2a65
              languageName: node
              linkType: hard

            "color-convert@npm:^2.0.1":
              version: 2.0.1
              resolution: "color-convert@npm:2.0.1"
              dependencies:
                color-name: ~1.1.4
              checksum: 3d5d8a011a43012ca11b6d739049ecf2055d95582fd16ec44bf1e685eb0baa5cc652002be8a1dc92b429c8d87418287d0528266a7595cb1ad8a7f4f1d3049df2
              languageName: node
              linkType: hard

            "color-name@npm:~1.1.4":
              version: 1.1.4
              resolution: "color-name@npm:1.1.4"
              checksum: 3e1c9a4dee12eada307436f61614dd11fe300469db2b83f80c8b7a7cd8a1015f0f18dd13403f018927b249003777ff60baba4a03c65f12e6bddc0dfd9642021f
              languageName: node
              linkType: hard

            "es-errors@npm:^1.3.0":
              version: 1.3.0
              resolution: "es-errors@npm:1.3.0"
              checksum: 46c70b612dd7441b73035ecc7ca7bd18695377bf1928a1d2676672d6fb0275ebf0a7b4ac0d40ceb2929a2ea54e877058b9ac77c15d2affa9910644e2cd5562c2
              languageName: node
              linkType: hard

            "function-bind@npm:^1.1.2":
              version: 1.1.2
              resolution: "function-bind@npm:1.1.2"
              checksum: 0d0c3d1dbe839454b444f2b9ee49cfdc35e40ed48c46658ad7f45d7e8621479adad93195fdd509860c5a339a69e2ffd9312d2e2ddb8671c248c9ff2942124468
              languageName: node
              linkType: hard

            "has-flag@npm:^4.0.0":
              version: 4.0.0
              resolution: "has-flag@npm:4.0.0"
              checksum: 2e5391139d3d287231ccb58659702392f6e3abeac3296fb4721afaff46493f3d9b99a9329ae015dfe973aa206ed5c75f43e86aec0267dce79aa5c2b6e811b3ad
              languageName: node
              linkType: hard

            "hasown@npm:^2.0.4":
              version: 2.0.4
              resolution: "hasown@npm:2.0.4"
              dependencies:
                function-bind: ^1.1.2
              checksum: 3a7736fae069b9f3d4dda622c703a4340d47596cdc016d5172aa51589a211091fe8db681fee60cfd28915ca034170ddadfd736398fd9c930d3ad6a286aa637c9
              languageName: node
              linkType: hard

            "is-core-module@npm:^2.17.0":
              version: 2.17.0
              resolution: "is-core-module@npm:2.17.0"
              dependencies:
                hasown: ^2.0.4
              checksum: 4dbc66559335738c59d81516c22f317d2e2549913f6ab2c1c6ff04796be412c2ba3ff26edd09197913e061e3b9fcb26f18ff78eadcdc10dd0ef6de77ebc055ce
              languageName: node
              linkType: hard

            "is-number@npm:^7.0.0":
              version: 7.0.0
              resolution: "is-number@npm:7.0.0"
              checksum: eec6e506c6de472af4bdfd0cc477e8aeb76f0a7066c8680fcdfed5324ee31a7d2b59d22313007c58aa80eb937f0c40eefdceedb851997d46b490b49f87160369
              languageName: node
              linkType: hard

            "isnum@npm:is-number@6.0.0":
              version: 6.0.0
              resolution: "is-number@npm:6.0.0"
              checksum: 691e7436801ab810c3be0289c425338115fcb15145923c47c8a5a2b600ce682804f257de17b81c6bba967f6c38034db672afde1caea96ff57928e4684988e41b
              languageName: node
              linkType: hard

            "path-parse@npm:^1.0.7":
              version: 1.0.7
              resolution: "path-parse@npm:1.0.7"
              checksum: 6de0bfa37b4f09af465ff3900fb4104ca9cb1e1fa5cbe869c40cedd10d5d625d04c284afc34967830eee780bf83fd69c017d72a23ffd35718ec861192ec91dd9
              languageName: node
              linkType: hard

            "pkg-a@workspace:packages/a":
              version: 0.0.0-use.local
              resolution: "pkg-a@workspace:packages/a"
              dependencies:
                chalk: 4.1.2
                pkg-b: "workspace:*"
              languageName: unknown
              linkType: soft

            "pkg-b@workspace:*, pkg-b@workspace:packages/b":
              version: 0.0.0-use.local
              resolution: "pkg-b@workspace:packages/b"
              dependencies:
                chalk: 4.1.0
              languageName: unknown
              linkType: soft

            resolve@^1.22.0:
              version: 1.22.13
              resolution: "resolve@npm:1.22.13"
              dependencies:
                es-errors: ^1.3.0
                is-core-module: ^2.17.0
                path-parse: ^1.0.7
                supports-preserve-symlinks-flag: ^1.0.0
              bin:
                resolve: bin/resolve
              checksum: 1dc8a913c4eb0065d5fcac981ce44224e2de317ff605dd2d8fa7e2181b25b27efece603cf25f58845429ba53056b1b7cdcc8a93c45f234bf7af5756143e2292e
              languageName: node
              linkType: hard

            "resolve@patch:resolve@^1.22.0#builtin<compat/resolve>":
              version: 1.22.13
              resolution: "resolve@patch:resolve@npm%3A1.22.13#builtin<compat/resolve>::version=1.22.13&hash=3388aa"
              dependencies:
                es-errors: ^1.3.0
                is-core-module: ^2.17.0
                path-parse: ^1.0.7
                supports-preserve-symlinks-flag: ^1.0.0
              bin:
                resolve: bin/resolve
              checksum: c06bd1d6e2441e3ec29fa35a09851679f6bf8d3cc19ec35addd4aa92f7214cff347d0604c23bd1b3b60073055ba8deda4a73c5cca6b8e49458f9fafb5c94c9f4
              languageName: node
              linkType: hard

            "supports-color@npm:^7.1.0":
              version: 7.2.0
              resolution: "supports-color@npm:7.2.0"
              dependencies:
                has-flag: ^4.0.0
              checksum: 8e57067c39216f3c2ffce7cc14ca934d54746192571203aa9c9922d97d2d55cc1bdaa9e41a11f91e620670b5a74ebdec6b548a885d8cc2dea7cab59e21416029
              languageName: node
              linkType: hard

            "supports-preserve-symlinks-flag@npm:^1.0.0":
              version: 1.0.0
              resolution: "supports-preserve-symlinks-flag@npm:1.0.0"
              checksum: a0f31404231c6450ae2d23acd7b066a31b9ca4649e02b6c70a4fbc955693cf65067507f450f43e4480df0f1fd87f72f78ed12c1a268a876c528be977c7096a69
              languageName: node
              linkType: hard
        "#},
    },
    SchemaLock {
        yarn: "3.1.1",
        schema: 5,
        lock: indoc! {r#"
            # This file is generated by running "yarn install" inside your project.
            # Manual changes might be lost - proceed with caution!

            __metadata:
              version: 5
              cacheKey: 8

            "@sindresorhus/is@npm:^4.0.0":
              version: 4.6.0
              resolution: "@sindresorhus/is@npm:4.6.0"
              checksum: 83839f13da2c29d55c97abc3bc2c55b250d33a0447554997a85c539e058e57b8da092da396e252b11ec24a0279a0bed1f537fa26302209327060643e327f81d2
              languageName: node
              linkType: hard

            "ansi-styles@npm:^4.1.0":
              version: 4.3.0
              resolution: "ansi-styles@npm:4.3.0"
              dependencies:
                color-convert: ^2.0.1
              checksum: 513b44c3b2105dd14cc42a19271e80f386466c4be574bccf60b627432f9198571ebf4ab1e4c3ba17347658f4ee1711c163d574248c0c1cdc2d5917a0ad582ec4
              languageName: node
              linkType: hard

            "berry-root@workspace:.":
              version: 0.0.0-use.local
              resolution: "berry-root@workspace:."
              dependencies:
                "@sindresorhus/is": ^4.0.0
                chalk: ^4.1.0
                is-number: ^7.0.0
                isnum: "npm:is-number@6.0.0"
                resolve: ^1.22.0
              languageName: unknown
              linkType: soft

            "chalk@npm:4.1.0":
              version: 4.1.0
              resolution: "chalk@npm:4.1.0"
              dependencies:
                ansi-styles: ^4.1.0
                supports-color: ^7.1.0
              checksum: 5561c7b4c063badee3e16d04bce50bd033e1be1bf4c6948639275683ffa7a1993c44639b43c22b1c505f0f813a24b1889037eb182546b48946f9fe7cdd0e7d13
              languageName: node
              linkType: hard

            "chalk@npm:4.1.2, chalk@npm:^4.1.0":
              version: 4.1.2
              resolution: "chalk@npm:4.1.2"
              dependencies:
                ansi-styles: ^4.1.0
                supports-color: ^7.1.0
              checksum: fe75c9d5c76a7a98d45495b91b2172fa3b7a09e0cc9370e5c8feb1c567b85c4288e2b3fded7cfdd7359ac28d6b3844feb8b82b8686842e93d23c827c417e83fc
              languageName: node
              linkType: hard

            "color-convert@npm:^2.0.1":
              version: 2.0.1
              resolution: "color-convert@npm:2.0.1"
              dependencies:
                color-name: ~1.1.4
              checksum: 79e6bdb9fd479a205c71d89574fccfb22bd9053bd98c6c4d870d65c132e5e904e6034978e55b43d69fcaa7433af2016ee203ce76eeba9cfa554b373e7f7db336
              languageName: node
              linkType: hard

            "color-name@npm:~1.1.4":
              version: 1.1.4
              resolution: "color-name@npm:1.1.4"
              checksum: b0445859521eb4021cd0fb0cc1a75cecf67fceecae89b63f62b201cca8d345baf8b952c966862a9d9a2632987d4f6581f0ec8d957dfacece86f0a7919316f610
              languageName: node
              linkType: hard

            "es-errors@npm:^1.3.0":
              version: 1.3.0
              resolution: "es-errors@npm:1.3.0"
              checksum: ec1414527a0ccacd7f15f4a3bc66e215f04f595ba23ca75cdae0927af099b5ec865f9f4d33e9d7e86f512f252876ac77d4281a7871531a50678132429b1271b5
              languageName: node
              linkType: hard

            "function-bind@npm:^1.1.2":
              version: 1.1.2
              resolution: "function-bind@npm:1.1.2"
              checksum: 2b0ff4ce708d99715ad14a6d1f894e2a83242e4a52ccfcefaee5e40050562e5f6dafc1adbb4ce2d4ab47279a45dc736ab91ea5042d843c3c092820dfe032efb1
              languageName: node
              linkType: hard

            "has-flag@npm:^4.0.0":
              version: 4.0.0
              resolution: "has-flag@npm:4.0.0"
              checksum: 261a1357037ead75e338156b1f9452c016a37dcd3283a972a30d9e4a87441ba372c8b81f818cd0fbcd9c0354b4ae7e18b9e1afa1971164aef6d18c2b6095a8ad
              languageName: node
              linkType: hard

            "hasown@npm:^2.0.4":
              version: 2.0.4
              resolution: "hasown@npm:2.0.4"
              dependencies:
                function-bind: ^1.1.2
              checksum: 4bd8f916b629e06324853593ffbdd45e200022952a85ad0c967f3bd4c2e4c7e1f9a9766fbe6186f60bd394e0afc73e719730caa1da15cd9bd832b7cdf53fd26c
              languageName: node
              linkType: hard

            "is-core-module@npm:^2.17.0":
              version: 2.17.0
              resolution: "is-core-module@npm:2.17.0"
              dependencies:
                hasown: ^2.0.4
              checksum: ab8a122dd92fc05b54e4f46c9859d6f8a9d9f20676a1b029605a4c33b151f07d8cd280e31f7806a29f29f8bfd8e3e647ba7b1cde7f07d60387c14be77774d346
              languageName: node
              linkType: hard

            "is-number@npm:^7.0.0":
              version: 7.0.0
              resolution: "is-number@npm:7.0.0"
              checksum: 456ac6f8e0f3111ed34668a624e45315201dff921e5ac181f8ec24923b99e9f32ca1a194912dc79d539c97d33dba17dc635202ff0b2cf98326f608323276d27a
              languageName: node
              linkType: hard

            "isnum@npm:is-number@6.0.0":
              version: 6.0.0
              resolution: "is-number@npm:6.0.0"
              checksum: f73bfced022128b5684bf77e0266a74e5222522bbc40f81cc1e949170c774a3c14b59a208be025d2d97a9c6b79c7c45fe351ab1c2c780872464fdedde0ae067a
              languageName: node
              linkType: hard

            "path-parse@npm:^1.0.7":
              version: 1.0.7
              resolution: "path-parse@npm:1.0.7"
              checksum: 49abf3d81115642938a8700ec580da6e830dde670be21893c62f4e10bd7dd4c3742ddc603fe24f898cba7eb0c6bc1777f8d9ac14185d34540c6d4d80cd9cae8a
              languageName: node
              linkType: hard

            "pkg-a@workspace:packages/a":
              version: 0.0.0-use.local
              resolution: "pkg-a@workspace:packages/a"
              dependencies:
                chalk: 4.1.2
                pkg-b: "workspace:*"
              languageName: unknown
              linkType: soft

            "pkg-b@workspace:*, pkg-b@workspace:packages/b":
              version: 0.0.0-use.local
              resolution: "pkg-b@workspace:packages/b"
              dependencies:
                chalk: 4.1.0
              languageName: unknown
              linkType: soft

            "resolve@npm:^1.22.0":
              version: 1.22.13
              resolution: "resolve@npm:1.22.13"
              dependencies:
                es-errors: ^1.3.0
                is-core-module: ^2.17.0
                path-parse: ^1.0.7
                supports-preserve-symlinks-flag: ^1.0.0
              bin:
                resolve: bin/resolve
              checksum: a661b1755171f58a6c45c1d242db0e8555b9b7530b939318a041612511127f185012ef2769807c0c7cd3e249b0262a938fd100bb6f2596efaa8b921f5ce58c49
              languageName: node
              linkType: hard

            "resolve@patch:resolve@^1.22.0#~builtin<compat/resolve>":
              version: 1.22.13
              resolution: "resolve@patch:resolve@npm%3A1.22.13#~builtin<compat/resolve>::version=1.22.13&hash=07638b"
              dependencies:
                es-errors: ^1.3.0
                is-core-module: ^2.17.0
                path-parse: ^1.0.7
                supports-preserve-symlinks-flag: ^1.0.0
              bin:
                resolve: bin/resolve
              checksum: b7766481246c1ccac341e5971e06e628e1ec4118dc8c7e2a97b29316bce7440913af1f4c7cc3044ced74161c6af55301879e7cd6a67bbb55618277ae457d20f3
              languageName: node
              linkType: hard

            "supports-color@npm:^7.1.0":
              version: 7.2.0
              resolution: "supports-color@npm:7.2.0"
              dependencies:
                has-flag: ^4.0.0
              checksum: 3dda818de06ebbe5b9653e07842d9479f3555ebc77e9a0280caf5a14fb877ffee9ed57007c3b78f5a6324b8dbeec648d9e97a24e2ed9fdb81ddc69ea07100f4a
              languageName: node
              linkType: hard

            "supports-preserve-symlinks-flag@npm:^1.0.0":
              version: 1.0.0
              resolution: "supports-preserve-symlinks-flag@npm:1.0.0"
              checksum: 53b1e247e68e05db7b3808b99b892bd36fb096e6fba213a06da7fab22045e97597db425c724f2bbd6c99a3c295e1e73f3e4de78592289f38431049e1277ca0ae
              languageName: node
              linkType: hard
        "#},
    },
    SchemaLock {
        yarn: "3.8.7",
        schema: 6,
        lock: indoc! {r#"
            # This file is generated by running "yarn install" inside your project.
            # Manual changes might be lost - proceed with caution!

            __metadata:
              version: 6
              cacheKey: 8

            "@sindresorhus/is@npm:^4.0.0":
              version: 4.6.0
              resolution: "@sindresorhus/is@npm:4.6.0"
              checksum: 83839f13da2c29d55c97abc3bc2c55b250d33a0447554997a85c539e058e57b8da092da396e252b11ec24a0279a0bed1f537fa26302209327060643e327f81d2
              languageName: node
              linkType: hard

            "ansi-styles@npm:^4.1.0":
              version: 4.3.0
              resolution: "ansi-styles@npm:4.3.0"
              dependencies:
                color-convert: ^2.0.1
              checksum: 513b44c3b2105dd14cc42a19271e80f386466c4be574bccf60b627432f9198571ebf4ab1e4c3ba17347658f4ee1711c163d574248c0c1cdc2d5917a0ad582ec4
              languageName: node
              linkType: hard

            "berry-root@workspace:.":
              version: 0.0.0-use.local
              resolution: "berry-root@workspace:."
              dependencies:
                "@sindresorhus/is": ^4.0.0
                chalk: ^4.1.0
                is-number: ^7.0.0
                isnum: "npm:is-number@6.0.0"
                resolve: ^1.22.0
              languageName: unknown
              linkType: soft

            "chalk@npm:4.1.0":
              version: 4.1.0
              resolution: "chalk@npm:4.1.0"
              dependencies:
                ansi-styles: ^4.1.0
                supports-color: ^7.1.0
              checksum: 5561c7b4c063badee3e16d04bce50bd033e1be1bf4c6948639275683ffa7a1993c44639b43c22b1c505f0f813a24b1889037eb182546b48946f9fe7cdd0e7d13
              languageName: node
              linkType: hard

            "chalk@npm:4.1.2, chalk@npm:^4.1.0":
              version: 4.1.2
              resolution: "chalk@npm:4.1.2"
              dependencies:
                ansi-styles: ^4.1.0
                supports-color: ^7.1.0
              checksum: fe75c9d5c76a7a98d45495b91b2172fa3b7a09e0cc9370e5c8feb1c567b85c4288e2b3fded7cfdd7359ac28d6b3844feb8b82b8686842e93d23c827c417e83fc
              languageName: node
              linkType: hard

            "color-convert@npm:^2.0.1":
              version: 2.0.1
              resolution: "color-convert@npm:2.0.1"
              dependencies:
                color-name: ~1.1.4
              checksum: 79e6bdb9fd479a205c71d89574fccfb22bd9053bd98c6c4d870d65c132e5e904e6034978e55b43d69fcaa7433af2016ee203ce76eeba9cfa554b373e7f7db336
              languageName: node
              linkType: hard

            "color-name@npm:~1.1.4":
              version: 1.1.4
              resolution: "color-name@npm:1.1.4"
              checksum: b0445859521eb4021cd0fb0cc1a75cecf67fceecae89b63f62b201cca8d345baf8b952c966862a9d9a2632987d4f6581f0ec8d957dfacece86f0a7919316f610
              languageName: node
              linkType: hard

            "es-errors@npm:^1.3.0":
              version: 1.3.0
              resolution: "es-errors@npm:1.3.0"
              checksum: ec1414527a0ccacd7f15f4a3bc66e215f04f595ba23ca75cdae0927af099b5ec865f9f4d33e9d7e86f512f252876ac77d4281a7871531a50678132429b1271b5
              languageName: node
              linkType: hard

            "function-bind@npm:^1.1.2":
              version: 1.1.2
              resolution: "function-bind@npm:1.1.2"
              checksum: 2b0ff4ce708d99715ad14a6d1f894e2a83242e4a52ccfcefaee5e40050562e5f6dafc1adbb4ce2d4ab47279a45dc736ab91ea5042d843c3c092820dfe032efb1
              languageName: node
              linkType: hard

            "has-flag@npm:^4.0.0":
              version: 4.0.0
              resolution: "has-flag@npm:4.0.0"
              checksum: 261a1357037ead75e338156b1f9452c016a37dcd3283a972a30d9e4a87441ba372c8b81f818cd0fbcd9c0354b4ae7e18b9e1afa1971164aef6d18c2b6095a8ad
              languageName: node
              linkType: hard

            "hasown@npm:^2.0.4":
              version: 2.0.4
              resolution: "hasown@npm:2.0.4"
              dependencies:
                function-bind: ^1.1.2
              checksum: 4bd8f916b629e06324853593ffbdd45e200022952a85ad0c967f3bd4c2e4c7e1f9a9766fbe6186f60bd394e0afc73e719730caa1da15cd9bd832b7cdf53fd26c
              languageName: node
              linkType: hard

            "is-core-module@npm:^2.17.0":
              version: 2.17.0
              resolution: "is-core-module@npm:2.17.0"
              dependencies:
                hasown: ^2.0.4
              checksum: ab8a122dd92fc05b54e4f46c9859d6f8a9d9f20676a1b029605a4c33b151f07d8cd280e31f7806a29f29f8bfd8e3e647ba7b1cde7f07d60387c14be77774d346
              languageName: node
              linkType: hard

            "is-number@npm:^7.0.0":
              version: 7.0.0
              resolution: "is-number@npm:7.0.0"
              checksum: 456ac6f8e0f3111ed34668a624e45315201dff921e5ac181f8ec24923b99e9f32ca1a194912dc79d539c97d33dba17dc635202ff0b2cf98326f608323276d27a
              languageName: node
              linkType: hard

            "isnum@npm:is-number@6.0.0":
              version: 6.0.0
              resolution: "is-number@npm:6.0.0"
              checksum: f73bfced022128b5684bf77e0266a74e5222522bbc40f81cc1e949170c774a3c14b59a208be025d2d97a9c6b79c7c45fe351ab1c2c780872464fdedde0ae067a
              languageName: node
              linkType: hard

            "path-parse@npm:^1.0.7":
              version: 1.0.7
              resolution: "path-parse@npm:1.0.7"
              checksum: 49abf3d81115642938a8700ec580da6e830dde670be21893c62f4e10bd7dd4c3742ddc603fe24f898cba7eb0c6bc1777f8d9ac14185d34540c6d4d80cd9cae8a
              languageName: node
              linkType: hard

            "pkg-a@workspace:packages/a":
              version: 0.0.0-use.local
              resolution: "pkg-a@workspace:packages/a"
              dependencies:
                chalk: 4.1.2
                pkg-b: "workspace:*"
              languageName: unknown
              linkType: soft

            "pkg-b@workspace:*, pkg-b@workspace:packages/b":
              version: 0.0.0-use.local
              resolution: "pkg-b@workspace:packages/b"
              dependencies:
                chalk: 4.1.0
              languageName: unknown
              linkType: soft

            "resolve@npm:^1.22.0":
              version: 1.22.13
              resolution: "resolve@npm:1.22.13"
              dependencies:
                es-errors: ^1.3.0
                is-core-module: ^2.17.0
                path-parse: ^1.0.7
                supports-preserve-symlinks-flag: ^1.0.0
              bin:
                resolve: bin/resolve
              checksum: a661b1755171f58a6c45c1d242db0e8555b9b7530b939318a041612511127f185012ef2769807c0c7cd3e249b0262a938fd100bb6f2596efaa8b921f5ce58c49
              languageName: node
              linkType: hard

            "resolve@patch:resolve@^1.22.0#~builtin<compat/resolve>":
              version: 1.22.13
              resolution: "resolve@patch:resolve@npm%3A1.22.13#~builtin<compat/resolve>::version=1.22.13&hash=c3c19d"
              dependencies:
                es-errors: ^1.3.0
                is-core-module: ^2.17.0
                path-parse: ^1.0.7
                supports-preserve-symlinks-flag: ^1.0.0
              bin:
                resolve: bin/resolve
              checksum: b7766481246c1ccac341e5971e06e628e1ec4118dc8c7e2a97b29316bce7440913af1f4c7cc3044ced74161c6af55301879e7cd6a67bbb55618277ae457d20f3
              languageName: node
              linkType: hard

            "supports-color@npm:^7.1.0":
              version: 7.2.0
              resolution: "supports-color@npm:7.2.0"
              dependencies:
                has-flag: ^4.0.0
              checksum: 3dda818de06ebbe5b9653e07842d9479f3555ebc77e9a0280caf5a14fb877ffee9ed57007c3b78f5a6324b8dbeec648d9e97a24e2ed9fdb81ddc69ea07100f4a
              languageName: node
              linkType: hard

            "supports-preserve-symlinks-flag@npm:^1.0.0":
              version: 1.0.0
              resolution: "supports-preserve-symlinks-flag@npm:1.0.0"
              checksum: 53b1e247e68e05db7b3808b99b892bd36fb096e6fba213a06da7fab22045e97597db425c724f2bbd6c99a3c295e1e73f3e4de78592289f38431049e1277ca0ae
              languageName: node
              linkType: hard
        "#},
    },
    SchemaLock {
        yarn: "4.0.2",
        schema: 8,
        lock: indoc! {r#"
            # This file is generated by running "yarn install" inside your project.
            # Manual changes might be lost - proceed with caution!

            __metadata:
              version: 8
              cacheKey: 10c0

            "@sindresorhus/is@npm:^4.0.0":
              version: 4.6.0
              resolution: "@sindresorhus/is@npm:4.6.0"
              checksum: 33b6fb1d0834ec8dd7689ddc0e2781c2bfd8b9c4e4bacbcb14111e0ae00621f2c264b8a7d36541799d74888b5dccdf422a891a5cb5a709ace26325eedc81e22e
              languageName: node
              linkType: hard

            "ansi-styles@npm:^4.1.0":
              version: 4.3.0
              resolution: "ansi-styles@npm:4.3.0"
              dependencies:
                color-convert: "npm:^2.0.1"
              checksum: 895a23929da416f2bd3de7e9cb4eabd340949328ab85ddd6e484a637d8f6820d485f53933446f5291c3b760cbc488beb8e88573dd0f9c7daf83dccc8fe81b041
              languageName: node
              linkType: hard

            "berry-root@workspace:.":
              version: 0.0.0-use.local
              resolution: "berry-root@workspace:."
              dependencies:
                "@sindresorhus/is": "npm:^4.0.0"
                chalk: "npm:^4.1.0"
                is-number: "npm:^7.0.0"
                isnum: "npm:is-number@6.0.0"
                resolve: "npm:^1.22.0"
              languageName: unknown
              linkType: soft

            "chalk@npm:4.1.0":
              version: 4.1.0
              resolution: "chalk@npm:4.1.0"
              dependencies:
                ansi-styles: "npm:^4.1.0"
                supports-color: "npm:^7.1.0"
              checksum: 3787bd65ecd98ab3a1acc3b4f71d006268a675875e49ee6ea75fb54ba73d268b97544368358c18c42445e408e076ae8ad5cec8fbad36942a2c7ac654883dc61e
              languageName: node
              linkType: hard

            "chalk@npm:4.1.2, chalk@npm:^4.1.0":
              version: 4.1.2
              resolution: "chalk@npm:4.1.2"
              dependencies:
                ansi-styles: "npm:^4.1.0"
                supports-color: "npm:^7.1.0"
              checksum: 4a3fef5cc34975c898ffe77141450f679721df9dde00f6c304353fa9c8b571929123b26a0e4617bde5018977eb655b31970c297b91b63ee83bb82aeb04666880
              languageName: node
              linkType: hard

            "color-convert@npm:^2.0.1":
              version: 2.0.1
              resolution: "color-convert@npm:2.0.1"
              dependencies:
                color-name: "npm:~1.1.4"
              checksum: 37e1150172f2e311fe1b2df62c6293a342ee7380da7b9cfdba67ea539909afbd74da27033208d01d6d5cfc65ee7868a22e18d7e7648e004425441c0f8a15a7d7
              languageName: node
              linkType: hard

            "color-name@npm:~1.1.4":
              version: 1.1.4
              resolution: "color-name@npm:1.1.4"
              checksum: a1a3f914156960902f46f7f56bc62effc6c94e84b2cae157a526b1c1f74b677a47ec602bf68a61abfa2b42d15b7c5651c6dbe72a43af720bc588dff885b10f95
              languageName: node
              linkType: hard

            "es-errors@npm:^1.3.0":
              version: 1.3.0
              resolution: "es-errors@npm:1.3.0"
              checksum: 0a61325670072f98d8ae3b914edab3559b6caa980f08054a3b872052640d91da01d38df55df797fcc916389d77fc92b8d5906cf028f4db46d7e3003abecbca85
              languageName: node
              linkType: hard

            "function-bind@npm:^1.1.2":
              version: 1.1.2
              resolution: "function-bind@npm:1.1.2"
              checksum: d8680ee1e5fcd4c197e4ac33b2b4dce03c71f4d91717292785703db200f5c21f977c568d28061226f9b5900cbcd2c84463646134fd5337e7925e0942bc3f46d5
              languageName: node
              linkType: hard

            "has-flag@npm:^4.0.0":
              version: 4.0.0
              resolution: "has-flag@npm:4.0.0"
              checksum: 2e789c61b7888d66993e14e8331449e525ef42aac53c627cc53d1c3334e768bcb6abdc4f5f0de1478a25beec6f0bd62c7549058b7ac53e924040d4f301f02fd1
              languageName: node
              linkType: hard

            "hasown@npm:^2.0.4":
              version: 2.0.4
              resolution: "hasown@npm:2.0.4"
              dependencies:
                function-bind: "npm:^1.1.2"
              checksum: 2d8de939e270b70618f8cebb69746620db10617dbb495bc66ddad326955ea24d3ca4af133aff3eb7c1853e0218f867bc2b050ec26fe02e3aea58f880ffc5e506
              languageName: node
              linkType: hard

            "is-core-module@npm:^2.17.0":
              version: 2.17.0
              resolution: "is-core-module@npm:2.17.0"
              dependencies:
                hasown: "npm:^2.0.4"
              checksum: 3dc7c15ea42f846180cd2cdd57af62270e0a8d0ed31c17c85735d554089d39975e2e82e65fc7c8280a3c476ca3d0d09992d0860b4998abb769a277341feb6698
              languageName: node
              linkType: hard

            "is-number@npm:^7.0.0":
              version: 7.0.0
              resolution: "is-number@npm:7.0.0"
              checksum: b4686d0d3053146095ccd45346461bc8e53b80aeb7671cc52a4de02dbbf7dc0d1d2a986e2fe4ae206984b4d34ef37e8b795ebc4f4295c978373e6575e295d811
              languageName: node
              linkType: hard

            "isnum@npm:is-number@6.0.0":
              version: 6.0.0
              resolution: "is-number@npm:6.0.0"
              checksum: 5da4c68401529675c575878d2760d66f18eaef4b014858577f6003daf66488d7fe4eae684b1e8574e3fa1bb447c6c6c56b8491d2b4b3239da2d32e5f6f218008
              languageName: node
              linkType: hard

            "path-parse@npm:^1.0.7":
              version: 1.0.7
              resolution: "path-parse@npm:1.0.7"
              checksum: 11ce261f9d294cc7a58d6a574b7f1b935842355ec66fba3c3fd79e0f036462eaf07d0aa95bb74ff432f9afef97ce1926c720988c6a7451d8a584930ae7de86e1
              languageName: node
              linkType: hard

            "pkg-a@workspace:packages/a":
              version: 0.0.0-use.local
              resolution: "pkg-a@workspace:packages/a"
              dependencies:
                chalk: "npm:4.1.2"
                pkg-b: "workspace:*"
              languageName: unknown
              linkType: soft

            "pkg-b@workspace:*, pkg-b@workspace:packages/b":
              version: 0.0.0-use.local
              resolution: "pkg-b@workspace:packages/b"
              dependencies:
                chalk: "npm:4.1.0"
              languageName: unknown
              linkType: soft

            "resolve@npm:^1.22.0":
              version: 1.22.13
              resolution: "resolve@npm:1.22.13"
              dependencies:
                es-errors: "npm:^1.3.0"
                is-core-module: "npm:^2.17.0"
                path-parse: "npm:^1.0.7"
                supports-preserve-symlinks-flag: "npm:^1.0.0"
              bin:
                resolve: bin/resolve
              checksum: 8dac5b33a1f9f241500fcaf42f9033cdcaf3738c908a60a50a976ff2f6129063cbc5368b4092e42d2d264ad22aa94aaff207f3efa8d0185c68454439b11a1973
              languageName: node
              linkType: hard

            "resolve@patch:resolve@npm%3A^1.22.0#optional!builtin<compat/resolve>":
              version: 1.22.13
              resolution: "resolve@patch:resolve@npm%3A1.22.13#optional!builtin<compat/resolve>::version=1.22.13&hash=c3c19d"
              dependencies:
                es-errors: "npm:^1.3.0"
                is-core-module: "npm:^2.17.0"
                path-parse: "npm:^1.0.7"
                supports-preserve-symlinks-flag: "npm:^1.0.0"
              bin:
                resolve: bin/resolve
              checksum: 6f10a5245bc8203b5c4d42528c2e221d32b6d3d9f07bb6c264205459ef7dae8318783f0f06a0add8b5e23a9101fdbd2e56267324bf3cb151578e91cf2e00febb
              languageName: node
              linkType: hard

            "supports-color@npm:^7.1.0":
              version: 7.2.0
              resolution: "supports-color@npm:7.2.0"
              dependencies:
                has-flag: "npm:^4.0.0"
              checksum: afb4c88521b8b136b5f5f95160c98dee7243dc79d5432db7efc27efb219385bbc7d9427398e43dd6cc730a0f87d5085ce1652af7efbe391327bc0a7d0f7fc124
              languageName: node
              linkType: hard

            "supports-preserve-symlinks-flag@npm:^1.0.0":
              version: 1.0.0
              resolution: "supports-preserve-symlinks-flag@npm:1.0.0"
              checksum: 6c4032340701a9950865f7ae8ef38578d8d7053f5e10518076e6554a9381fa91bd9c6850193695c141f32b21f979c985db07265a758867bac95de05f7d8aeb39
              languageName: node
              linkType: hard
        "#},
    },
    SchemaLock {
        yarn: "4.14.1",
        schema: 9,
        lock: indoc! {r#"
            # This file is generated by running "yarn install" inside your project.
            # Manual changes might be lost - proceed with caution!

            __metadata:
              version: 9
              cacheKey: 10c0

            "@sindresorhus/is@npm:^4.0.0":
              version: 4.6.0
              resolution: "@sindresorhus/is@npm:4.6.0"
              checksum: 10c0/33b6fb1d0834ec8dd7689ddc0e2781c2bfd8b9c4e4bacbcb14111e0ae00621f2c264b8a7d36541799d74888b5dccdf422a891a5cb5a709ace26325eedc81e22e
              languageName: node
              linkType: hard

            "ansi-styles@npm:^4.1.0":
              version: 4.3.0
              resolution: "ansi-styles@npm:4.3.0"
              dependencies:
                color-convert: "npm:^2.0.1"
              checksum: 10c0/895a23929da416f2bd3de7e9cb4eabd340949328ab85ddd6e484a637d8f6820d485f53933446f5291c3b760cbc488beb8e88573dd0f9c7daf83dccc8fe81b041
              languageName: node
              linkType: hard

            "berry-root@workspace:.":
              version: 0.0.0-use.local
              resolution: "berry-root@workspace:."
              dependencies:
                "@sindresorhus/is": "npm:^4.0.0"
                chalk: "npm:^4.1.0"
                is-number: "npm:^7.0.0"
                isnum: "npm:is-number@6.0.0"
                resolve: "npm:^1.22.0"
              languageName: unknown
              linkType: soft

            "chalk@npm:4.1.0":
              version: 4.1.0
              resolution: "chalk@npm:4.1.0"
              dependencies:
                ansi-styles: "npm:^4.1.0"
                supports-color: "npm:^7.1.0"
              checksum: 10c0/3787bd65ecd98ab3a1acc3b4f71d006268a675875e49ee6ea75fb54ba73d268b97544368358c18c42445e408e076ae8ad5cec8fbad36942a2c7ac654883dc61e
              languageName: node
              linkType: hard

            "chalk@npm:4.1.2, chalk@npm:^4.1.0":
              version: 4.1.2
              resolution: "chalk@npm:4.1.2"
              dependencies:
                ansi-styles: "npm:^4.1.0"
                supports-color: "npm:^7.1.0"
              checksum: 10c0/4a3fef5cc34975c898ffe77141450f679721df9dde00f6c304353fa9c8b571929123b26a0e4617bde5018977eb655b31970c297b91b63ee83bb82aeb04666880
              languageName: node
              linkType: hard

            "color-convert@npm:^2.0.1":
              version: 2.0.1
              resolution: "color-convert@npm:2.0.1"
              dependencies:
                color-name: "npm:~1.1.4"
              checksum: 10c0/37e1150172f2e311fe1b2df62c6293a342ee7380da7b9cfdba67ea539909afbd74da27033208d01d6d5cfc65ee7868a22e18d7e7648e004425441c0f8a15a7d7
              languageName: node
              linkType: hard

            "color-name@npm:~1.1.4":
              version: 1.1.4
              resolution: "color-name@npm:1.1.4"
              checksum: 10c0/a1a3f914156960902f46f7f56bc62effc6c94e84b2cae157a526b1c1f74b677a47ec602bf68a61abfa2b42d15b7c5651c6dbe72a43af720bc588dff885b10f95
              languageName: node
              linkType: hard

            "es-errors@npm:^1.3.0":
              version: 1.3.0
              resolution: "es-errors@npm:1.3.0"
              checksum: 10c0/0a61325670072f98d8ae3b914edab3559b6caa980f08054a3b872052640d91da01d38df55df797fcc916389d77fc92b8d5906cf028f4db46d7e3003abecbca85
              languageName: node
              linkType: hard

            "function-bind@npm:^1.1.2":
              version: 1.1.2
              resolution: "function-bind@npm:1.1.2"
              checksum: 10c0/d8680ee1e5fcd4c197e4ac33b2b4dce03c71f4d91717292785703db200f5c21f977c568d28061226f9b5900cbcd2c84463646134fd5337e7925e0942bc3f46d5
              languageName: node
              linkType: hard

            "has-flag@npm:^4.0.0":
              version: 4.0.0
              resolution: "has-flag@npm:4.0.0"
              checksum: 10c0/2e789c61b7888d66993e14e8331449e525ef42aac53c627cc53d1c3334e768bcb6abdc4f5f0de1478a25beec6f0bd62c7549058b7ac53e924040d4f301f02fd1
              languageName: node
              linkType: hard

            "hasown@npm:^2.0.4":
              version: 2.0.4
              resolution: "hasown@npm:2.0.4"
              dependencies:
                function-bind: "npm:^1.1.2"
              checksum: 10c0/2d8de939e270b70618f8cebb69746620db10617dbb495bc66ddad326955ea24d3ca4af133aff3eb7c1853e0218f867bc2b050ec26fe02e3aea58f880ffc5e506
              languageName: node
              linkType: hard

            "is-core-module@npm:^2.17.0":
              version: 2.17.0
              resolution: "is-core-module@npm:2.17.0"
              dependencies:
                hasown: "npm:^2.0.4"
              checksum: 10c0/3dc7c15ea42f846180cd2cdd57af62270e0a8d0ed31c17c85735d554089d39975e2e82e65fc7c8280a3c476ca3d0d09992d0860b4998abb769a277341feb6698
              languageName: node
              linkType: hard

            "is-number@npm:^7.0.0":
              version: 7.0.0
              resolution: "is-number@npm:7.0.0"
              checksum: 10c0/b4686d0d3053146095ccd45346461bc8e53b80aeb7671cc52a4de02dbbf7dc0d1d2a986e2fe4ae206984b4d34ef37e8b795ebc4f4295c978373e6575e295d811
              languageName: node
              linkType: hard

            "isnum@npm:is-number@6.0.0":
              version: 6.0.0
              resolution: "is-number@npm:6.0.0"
              checksum: 10c0/5da4c68401529675c575878d2760d66f18eaef4b014858577f6003daf66488d7fe4eae684b1e8574e3fa1bb447c6c6c56b8491d2b4b3239da2d32e5f6f218008
              languageName: node
              linkType: hard

            "path-parse@npm:^1.0.7":
              version: 1.0.7
              resolution: "path-parse@npm:1.0.7"
              checksum: 10c0/11ce261f9d294cc7a58d6a574b7f1b935842355ec66fba3c3fd79e0f036462eaf07d0aa95bb74ff432f9afef97ce1926c720988c6a7451d8a584930ae7de86e1
              languageName: node
              linkType: hard

            "pkg-a@workspace:packages/a":
              version: 0.0.0-use.local
              resolution: "pkg-a@workspace:packages/a"
              dependencies:
                chalk: "npm:4.1.2"
                pkg-b: "workspace:*"
              languageName: unknown
              linkType: soft

            "pkg-b@workspace:*, pkg-b@workspace:packages/b":
              version: 0.0.0-use.local
              resolution: "pkg-b@workspace:packages/b"
              dependencies:
                chalk: "npm:4.1.0"
              languageName: unknown
              linkType: soft

            "resolve@npm:^1.22.0":
              version: 1.22.13
              resolution: "resolve@npm:1.22.13"
              dependencies:
                es-errors: "npm:^1.3.0"
                is-core-module: "npm:^2.17.0"
                path-parse: "npm:^1.0.7"
                supports-preserve-symlinks-flag: "npm:^1.0.0"
              bin:
                resolve: bin/resolve
              checksum: 10c0/8dac5b33a1f9f241500fcaf42f9033cdcaf3738c908a60a50a976ff2f6129063cbc5368b4092e42d2d264ad22aa94aaff207f3efa8d0185c68454439b11a1973
              languageName: node
              linkType: hard

            "resolve@patch:resolve@npm%3A^1.22.0#optional!builtin<compat/resolve>":
              version: 1.22.13
              resolution: "resolve@patch:resolve@npm%3A1.22.13#optional!builtin<compat/resolve>::version=1.22.13&hash=c3c19d"
              dependencies:
                es-errors: "npm:^1.3.0"
                is-core-module: "npm:^2.17.0"
                path-parse: "npm:^1.0.7"
                supports-preserve-symlinks-flag: "npm:^1.0.0"
              bin:
                resolve: bin/resolve
              checksum: 10c0/6f10a5245bc8203b5c4d42528c2e221d32b6d3d9f07bb6c264205459ef7dae8318783f0f06a0add8b5e23a9101fdbd2e56267324bf3cb151578e91cf2e00febb
              languageName: node
              linkType: hard

            "supports-color@npm:^7.1.0":
              version: 7.2.0
              resolution: "supports-color@npm:7.2.0"
              dependencies:
                has-flag: "npm:^4.0.0"
              checksum: 10c0/afb4c88521b8b136b5f5f95160c98dee7243dc79d5432db7efc27efb219385bbc7d9427398e43dd6cc730a0f87d5085ce1652af7efbe391327bc0a7d0f7fc124
              languageName: node
              linkType: hard

            "supports-preserve-symlinks-flag@npm:^1.0.0":
              version: 1.0.0
              resolution: "supports-preserve-symlinks-flag@npm:1.0.0"
              checksum: 10c0/6c4032340701a9950865f7ae8ef38578d8d7053f5e10518076e6554a9381fa91bd9c6850193695c141f32b21f979c985db07265a758867bac95de05f7d8aeb39
              languageName: node
              linkType: hard
        "#},
    },
    SchemaLock {
        yarn: "4.18.1",
        schema: 10,
        lock: indoc! {r#"
            # This file is generated by running "yarn install" inside your project.
            # Manual changes might be lost - proceed with caution!

            __metadata:
              version: 10
              cacheKey: 10c0

            "@sindresorhus/is@npm:^4.0.0":
              version: 4.6.0
              resolution: "@sindresorhus/is@npm:4.6.0"
              checksum: 10c0/33b6fb1d0834ec8dd7689ddc0e2781c2bfd8b9c4e4bacbcb14111e0ae00621f2c264b8a7d36541799d74888b5dccdf422a891a5cb5a709ace26325eedc81e22e
              languageName: node
              linkType: hard

            "ansi-styles@npm:^4.1.0":
              version: 4.3.0
              resolution: "ansi-styles@npm:4.3.0"
              dependencies:
                color-convert: "npm:^2.0.1"
              checksum: 10c0/895a23929da416f2bd3de7e9cb4eabd340949328ab85ddd6e484a637d8f6820d485f53933446f5291c3b760cbc488beb8e88573dd0f9c7daf83dccc8fe81b041
              languageName: node
              linkType: hard

            "berry-root@workspace:.":
              version: 0.0.0-use.local
              resolution: "berry-root@workspace:."
              dependencies:
                "@sindresorhus/is": "npm:^4.0.0"
                chalk: "npm:^4.1.0"
                is-number: "npm:^7.0.0"
                isnum: "npm:is-number@6.0.0"
                resolve: "npm:^1.22.0"
              languageName: unknown
              linkType: soft

            "chalk@npm:4.1.0":
              version: 4.1.0
              resolution: "chalk@npm:4.1.0"
              dependencies:
                ansi-styles: "npm:^4.1.0"
                supports-color: "npm:^7.1.0"
              checksum: 10c0/3787bd65ecd98ab3a1acc3b4f71d006268a675875e49ee6ea75fb54ba73d268b97544368358c18c42445e408e076ae8ad5cec8fbad36942a2c7ac654883dc61e
              languageName: node
              linkType: hard

            "chalk@npm:4.1.2, chalk@npm:^4.1.0":
              version: 4.1.2
              resolution: "chalk@npm:4.1.2"
              dependencies:
                ansi-styles: "npm:^4.1.0"
                supports-color: "npm:^7.1.0"
              checksum: 10c0/4a3fef5cc34975c898ffe77141450f679721df9dde00f6c304353fa9c8b571929123b26a0e4617bde5018977eb655b31970c297b91b63ee83bb82aeb04666880
              languageName: node
              linkType: hard

            "color-convert@npm:^2.0.1":
              version: 2.0.1
              resolution: "color-convert@npm:2.0.1"
              dependencies:
                color-name: "npm:~1.1.4"
              checksum: 10c0/37e1150172f2e311fe1b2df62c6293a342ee7380da7b9cfdba67ea539909afbd74da27033208d01d6d5cfc65ee7868a22e18d7e7648e004425441c0f8a15a7d7
              languageName: node
              linkType: hard

            "color-name@npm:~1.1.4":
              version: 1.1.4
              resolution: "color-name@npm:1.1.4"
              checksum: 10c0/a1a3f914156960902f46f7f56bc62effc6c94e84b2cae157a526b1c1f74b677a47ec602bf68a61abfa2b42d15b7c5651c6dbe72a43af720bc588dff885b10f95
              languageName: node
              linkType: hard

            "es-errors@npm:^1.3.0":
              version: 1.3.0
              resolution: "es-errors@npm:1.3.0"
              checksum: 10c0/0a61325670072f98d8ae3b914edab3559b6caa980f08054a3b872052640d91da01d38df55df797fcc916389d77fc92b8d5906cf028f4db46d7e3003abecbca85
              languageName: node
              linkType: hard

            "function-bind@npm:^1.1.2":
              version: 1.1.2
              resolution: "function-bind@npm:1.1.2"
              checksum: 10c0/d8680ee1e5fcd4c197e4ac33b2b4dce03c71f4d91717292785703db200f5c21f977c568d28061226f9b5900cbcd2c84463646134fd5337e7925e0942bc3f46d5
              languageName: node
              linkType: hard

            "has-flag@npm:^4.0.0":
              version: 4.0.0
              resolution: "has-flag@npm:4.0.0"
              checksum: 10c0/2e789c61b7888d66993e14e8331449e525ef42aac53c627cc53d1c3334e768bcb6abdc4f5f0de1478a25beec6f0bd62c7549058b7ac53e924040d4f301f02fd1
              languageName: node
              linkType: hard

            "hasown@npm:^2.0.4":
              version: 2.0.4
              resolution: "hasown@npm:2.0.4"
              dependencies:
                function-bind: "npm:^1.1.2"
              checksum: 10c0/2d8de939e270b70618f8cebb69746620db10617dbb495bc66ddad326955ea24d3ca4af133aff3eb7c1853e0218f867bc2b050ec26fe02e3aea58f880ffc5e506
              languageName: node
              linkType: hard

            "is-core-module@npm:^2.17.0":
              version: 2.17.0
              resolution: "is-core-module@npm:2.17.0"
              dependencies:
                hasown: "npm:^2.0.4"
              checksum: 10c0/3dc7c15ea42f846180cd2cdd57af62270e0a8d0ed31c17c85735d554089d39975e2e82e65fc7c8280a3c476ca3d0d09992d0860b4998abb769a277341feb6698
              languageName: node
              linkType: hard

            "is-number@npm:^7.0.0":
              version: 7.0.0
              resolution: "is-number@npm:7.0.0"
              checksum: 10c0/b4686d0d3053146095ccd45346461bc8e53b80aeb7671cc52a4de02dbbf7dc0d1d2a986e2fe4ae206984b4d34ef37e8b795ebc4f4295c978373e6575e295d811
              languageName: node
              linkType: hard

            "isnum@npm:is-number@6.0.0":
              version: 6.0.0
              resolution: "is-number@npm:6.0.0"
              checksum: 10c0/5da4c68401529675c575878d2760d66f18eaef4b014858577f6003daf66488d7fe4eae684b1e8574e3fa1bb447c6c6c56b8491d2b4b3239da2d32e5f6f218008
              languageName: node
              linkType: hard

            "path-parse@npm:^1.0.7":
              version: 1.0.7
              resolution: "path-parse@npm:1.0.7"
              checksum: 10c0/11ce261f9d294cc7a58d6a574b7f1b935842355ec66fba3c3fd79e0f036462eaf07d0aa95bb74ff432f9afef97ce1926c720988c6a7451d8a584930ae7de86e1
              languageName: node
              linkType: hard

            "pkg-a@workspace:packages/a":
              version: 0.0.0-use.local
              resolution: "pkg-a@workspace:packages/a"
              dependencies:
                chalk: "npm:4.1.2"
                pkg-b: "workspace:*"
              languageName: unknown
              linkType: soft

            "pkg-b@workspace:*, pkg-b@workspace:packages/b":
              version: 0.0.0-use.local
              resolution: "pkg-b@workspace:packages/b"
              dependencies:
                chalk: "npm:4.1.0"
              languageName: unknown
              linkType: soft

            "resolve@npm:^1.22.0":
              version: 1.22.13
              resolution: "resolve@npm:1.22.13"
              dependencies:
                es-errors: "npm:^1.3.0"
                is-core-module: "npm:^2.17.0"
                path-parse: "npm:^1.0.7"
                supports-preserve-symlinks-flag: "npm:^1.0.0"
              bin:
                resolve: bin/resolve
              checksum: 10c0/8dac5b33a1f9f241500fcaf42f9033cdcaf3738c908a60a50a976ff2f6129063cbc5368b4092e42d2d264ad22aa94aaff207f3efa8d0185c68454439b11a1973
              languageName: node
              linkType: hard

            "resolve@patch:resolve@npm%3A^1.22.0#optional!builtin<compat/resolve>":
              version: 1.22.13
              resolution: "resolve@patch:resolve@npm%3A1.22.13#optional!builtin<compat/resolve>::version=1.22.13&hash=9bd1a5"
              dependencies:
                es-errors: "npm:^1.3.0"
                is-core-module: "npm:^2.17.0"
                path-parse: "npm:^1.0.7"
                supports-preserve-symlinks-flag: "npm:^1.0.0"
              bin:
                resolve: bin/resolve
              checksum: 10c0/bc58ee02e647a4250453eeb067796bdc087f6a89d4d5780bfe479f402ec11b9461babc155a6452020b56f3845cf49b1afd9c95fe3fc8347cba6567aa9b10c106
              languageName: node
              linkType: hard

            "supports-color@npm:^7.1.0":
              version: 7.2.0
              resolution: "supports-color@npm:7.2.0"
              dependencies:
                has-flag: "npm:^4.0.0"
              checksum: 10c0/afb4c88521b8b136b5f5f95160c98dee7243dc79d5432db7efc27efb219385bbc7d9427398e43dd6cc730a0f87d5085ce1652af7efbe391327bc0a7d0f7fc124
              languageName: node
              linkType: hard

            "supports-preserve-symlinks-flag@npm:^1.0.0":
              version: 1.0.0
              resolution: "supports-preserve-symlinks-flag@npm:1.0.0"
              checksum: 10c0/6c4032340701a9950865f7ae8ef38578d8d7053f5e10518076e6554a9381fa91bd9c6850193695c141f32b21f979c985db07265a758867bac95de05f7d8aeb39
              languageName: node
              linkType: hard
        "#},
    },
];
