# Changelog

## [1.1.0](https://github.com/OmentaElvis/skot/compare/v1.0.0...v1.1.0) (2026-10-29)

### Features

* **ci:** wire up docker and gh actions ([5c3ee11](https://github.com/OmentaElvis/skot/commit/5c3ee1167a4311179734f7d0be707183f94e1a5a))

## 1.0.0 (2026-10-29)

### Features

* --pr reads the forge the request comes from ([097a077](https://github.com/OmentaElvis/skot/commit/097a07773a685c477813cd013f6a01d95ea145dc))
* --refresh and --offline belong to the run, and the origin is asked once ([2f27f2c](https://github.com/OmentaElvis/skot/commit/2f27f2c41e4fd337340b321929602b2fc32a1b45))
* --repo reads sources where they lie, and every ref chain falls back to the mirror's HEAD ([858537d](https://github.com/OmentaElvis/skot/commit/858537dda5ec1637763798e531b0e8d6f93346b3))
* add confirmation for patch deletion ([5eee33a](https://github.com/OmentaElvis/skot/commit/5eee33aa73351f621f4d05a5f92b47f8159a3ac7))
* add enable/disable commands ([bd4d0ed](https://github.com/OmentaElvis/skot/commit/bd4d0ed5488b3a7f19c4769dd7c67420fb709c9d))
* add imports subcommand and cargo fmt ([51fce0b](https://github.com/OmentaElvis/skot/commit/51fce0bacd8430869f4f70cb33864855921be588))
* add logo to skot and update description ([00c4cf2](https://github.com/OmentaElvis/skot/commit/00c4cf24558c51db0827ade95d28882e72dbc3ab))
* add more clap env variables for shared cli flags ([7cda74d](https://github.com/OmentaElvis/skot/commit/7cda74d5929e6a937c814ea3c671a47293c5357f))
* add move subcommand ([60aaefd](https://github.com/OmentaElvis/skot/commit/60aaefdcdbbd0260543e10e4e7f34034beff69c4))
* add patch show, a patch printer for terminals, pipes and machines ([3be8fd6](https://github.com/OmentaElvis/skot/commit/3be8fd61cfe81fb7b88d2cef2228d2fae986ef94))
* add README docs ([783fb8b](https://github.com/OmentaElvis/skot/commit/783fb8b9b2393fff2d71cc7c43b05622901bcb76))
* added ability to remove patches and patch reordering ([56061a8](https://github.com/OmentaElvis/skot/commit/56061a86078c313d0f7afb675a34a636e256fbb1))
* **blobs:** support binary files with a shared blob store ([d0d4df8](https://github.com/OmentaElvis/skot/commit/d0d4df855e7ee9ba466e50ac1b1f8044e80e0cd5))
* **ci:** configure ci build ([0db9a88](https://github.com/OmentaElvis/skot/commit/0db9a88d178758569bb351538efa4ba05bff6d5a))
* general updates ([79e7660](https://github.com/OmentaElvis/skot/commit/79e7660f6f049c394065845563b52a74e7812a0c))
* initial commit ([31826c1](https://github.com/OmentaElvis/skot/commit/31826c15a600a5bd988d0106889e6e04ad83c3bc))
* **init:** write .gitignore for .skot by default ([c5ba544](https://github.com/OmentaElvis/skot/commit/c5ba544306fee28539807175377d2c890e886cca))
* make every command fetch what it needs, and nothing more ([c82dbe2](https://github.com/OmentaElvis/skot/commit/c82dbe2451ccfffb1a636fde28c1bd845d05dc73))
* multi-series support ([aef3716](https://github.com/OmentaElvis/skot/commit/aef371641c7e7f35461490e3262f9d4c66a3820e))
* reorganize commands into groups ([3e91d5a](https://github.com/OmentaElvis/skot/commit/3e91d5a40e44349e720530ff795a8b200d79320f))
* update prompt handling and styling ([940c426](https://github.com/OmentaElvis/skot/commit/940c426dc46848de563ec6b00f997952ff0eb3fe))

### Bug Fixes

* a work area directory is named after the patch, not the file ([ad42d61](https://github.com/OmentaElvis/skot/commit/ad42d6142f07c4e9fd13214be7f148c41b941314))
* an empty pop or push answers instead of erroring ([1434f97](https://github.com/OmentaElvis/skot/commit/1434f97000ba489b4eacf060a84bb3ec7a82339e))
* build continues on the ref the sandbox stands on and names that ref's own commit ([43fba41](https://github.com/OmentaElvis/skot/commit/43fba41677fc6eaab1b2a211dbdcffdabdbbf15a))
* build now respects pre applied patch order ([7419d56](https://github.com/OmentaElvis/skot/commit/7419d56136b159401005b816446245aa83edbe20))
* diagnose commit expressions and stale worktree registrations ([cd9b364](https://github.com/OmentaElvis/skot/commit/cd9b364da899b9fbc36785f6a6cfc73d58d2307e))
* hook cwd, hooks listing, edit resolution, init defaults, series default ([619220b](https://github.com/OmentaElvis/skot/commit/619220b2efa6ab2d994267b48c43c57c9b0d1c40))
* hookup -v flag ([73ac961](https://github.com/OmentaElvis/skot/commit/73ac96187dea442dd2e40a52c44bd6f508bc4d0b))
* **import:** stop re-importing the same change as a new patch ([6e74a1a](https://github.com/OmentaElvis/skot/commit/6e74a1a92958fb08afffe4f3ea17793f425c7036))
* json failures speak json, and a batch of small cli fixes ([e574e9d](https://github.com/OmentaElvis/skot/commit/e574e9d96c4fa212504bd231b49da1da6b013ec8))
* properly format anyhow errors ([015f4c3](https://github.com/OmentaElvis/skot/commit/015f4c32bf0879ea5b42cf9b7b42a472e97e2445))
* questions cannot block, enabled is not applied, and save ([bc1d112](https://github.com/OmentaElvis/skot/commit/bc1d112f12186739cae2f9c23758ac8aceebf3bc))
* rename 'series use' to 'series switch' ([40802c6](https://github.com/OmentaElvis/skot/commit/40802c69dbdf1c8f343b912dcbb72be918ed94f6))
* update/overhaul patch authoring ([6fd03b9](https://github.com/OmentaElvis/skot/commit/6fd03b9501a9c7aa91c2b0a6c6d48358f66a22f6))
* use fs4 for file locking ([0b6b9bc](https://github.com/OmentaElvis/skot/commit/0b6b9bcb8ee96c21e3b883682ab68a0b05388af4))
