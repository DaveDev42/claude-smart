# Changelog

## [0.2.10](https://github.com/DaveDev42/claude-smart/compare/v0.2.9...v0.2.10) (2026-06-29)


### Features

* **account:** usage max-age gate — stale 데이터로 auto-pick 금지 ([0bda824](https://github.com/DaveDev42/claude-smart/commit/0bda824da77f83f5ce912e6af86f63d1ca0ab342))

## [0.2.9](https://github.com/DaveDev42/claude-smart/compare/v0.2.8...v0.2.9) (2026-06-28)


### Features

* **config:** launch a configurable drop-in command instead of claude ([e3e2b5d](https://github.com/DaveDev42/claude-smart/commit/e3e2b5d1f7d9fd988f39a68f32ab7380e4b1924c))

## [0.2.8](https://github.com/DaveDev42/claude-smart/compare/v0.2.7...v0.2.8) (2026-06-28)


### Features

* **reaper:** add `csm reap` orphan-process discovery (Phase 1, dry-run only) ([9776463](https://github.com/DaveDev42/claude-smart/commit/9776463dd4e267d36118d0bfbb3d73a6b3fea9c6))
* **reaper:** interactive multi-select kill for csm reap (Phase 2) ([38dc41e](https://github.com/DaveDev42/claude-smart/commit/38dc41ebbe7b183091765bc3b2ca5d9f96c5094a))


### Bug Fixes

* **reaper:** Windows kill_one — HANDLE is isize in windows-sys 0.52 ([a398536](https://github.com/DaveDev42/claude-smart/commit/a3985368310e1bd16e5fad25e53690edf6ad9c24))

## [0.2.7](https://github.com/DaveDev42/claude-smart/compare/v0.2.6...v0.2.7) (2026-06-28)


### Bug Fixes

* **picker:** always show session picker, mark recommended account with ★ ([23b789e](https://github.com/DaveDev42/claude-smart/commit/23b789e43624f36f67a991b35568d0f26fd61332))

## [0.2.6](https://github.com/DaveDev42/claude-smart/compare/v0.2.5...v0.2.6) (2026-06-28)


### Bug Fixes

* **paths:** encode cwd with [^A-Za-z0-9]→- to match Claude Code ([9433fbb](https://github.com/DaveDev42/claude-smart/commit/9433fbb0010a8cce085e84250a5eaafe06afb162))

## [0.2.5](https://github.com/DaveDev42/claude-smart/compare/v0.2.4...v0.2.5) (2026-06-26)


### Features

* **picker:** usage 파악 불가 시 auto-pick skip 금지 + -i로 picker 강제 ([b2971b0](https://github.com/DaveDev42/claude-smart/commit/b2971b0d8c45d14817a6f79f4647d4e71c68b393))

## [0.2.4](https://github.com/DaveDev42/claude-smart/compare/v0.2.3...v0.2.4) (2026-06-25)


### Features

* **picker:** order hub-down account picker by recommendation ([c46eeec](https://github.com/DaveDev42/claude-smart/commit/c46eeec32a760379ff3f5719b20e0ad272e8c35f))

## [0.2.3](https://github.com/DaveDev42/claude-smart/compare/v0.2.2...v0.2.3) (2026-06-25)


### Bug Fixes

* **session:** resume existing sessions with --resume, not --session-id ([db554c6](https://github.com/DaveDev42/claude-smart/commit/db554c6bd8ac47c80b13c1c1fc0a25152d132c17))

## [0.2.2](https://github.com/DaveDev42/claude-smart/compare/v0.2.1...v0.2.2) (2026-06-24)


### Bug Fixes

* **picker:** treat Escape as cancel, not as proceed-with-default ([7bf85d6](https://github.com/DaveDev42/claude-smart/commit/7bf85d68fed76fdbff3b8f688798b543f812d10b))

## [0.2.1](https://github.com/DaveDev42/claude-smart/compare/v0.2.0...v0.2.1) (2026-06-23)


### Features

* **usage:** add CSM_USAGE_CMD pluggable usage source + configurable TTL ([26de714](https://github.com/DaveDev42/claude-smart/commit/26de714398822f8152bb4367d5c621da84099ee0))


### Bug Fixes

* **degraded:** graceful pick-account + clean profiles-list when no registry ([703bdb8](https://github.com/DaveDev42/claude-smart/commit/703bdb8689cc1837ca00e727ae5abf3a16e12fd0))
* **scan:** sanitize newlines/tabs in index fields; harden state read-compat ([c065518](https://github.com/DaveDev42/claude-smart/commit/c065518f5374087b0a6620f8f446ac13aa6c0b51))
* **test:** generate large deadlock fixture in-child to avoid Linux ARG_MAX ([cafcb5f](https://github.com/DaveDev42/claude-smart/commit/cafcb5fefc7e30ea8ef5b0ee5615589d6e9d5289))
* **usage:** drain CSM_USAGE_CMD stdout to avoid a pipe deadlock ([2c1c296](https://github.com/DaveDev42/claude-smart/commit/2c1c296cfff1775b95de92ec87bfdaff3b255ff4))
* **usage:** make CSM_USAGE_CMD timeout hard against a pipe-holding grandchild ([d96cd3a](https://github.com/DaveDev42/claude-smart/commit/d96cd3aa4a3dd1d412d4e5f13a15f17ad062ab30))
* **windows:** gate the unverified relaunch loop off, fall back to launch-once ([ba4a9f0](https://github.com/DaveDev42/claude-smart/commit/ba4a9f074e5b7a4df20c4b9c797ae316092aa3a8))
* **windows:** resolve dead_code under clippy -D warnings on windows-msvc ([b88fdcc](https://github.com/DaveDev42/claude-smart/commit/b88fdcc2ae4078c3c5f6779a116b971771655219))

## [0.1.1](https://github.com/DaveDev42/claude-smart/compare/v0.1.0...v0.1.1) (2026-06-17)


### Features

* phase 1 — pure core logic (parser, sidecar merge, account scoring, reset-epoch, usage transport, session scan) ([354ebb0](https://github.com/DaveDev42/claude-smart/commit/354ebb03874864187ae1c787522bba487c44dc2c))
* phase 2 — proc_check, liveness, fzf pickers (session + hub-down account) ([09de51b](https://github.com/DaveDev42/claude-smart/commit/09de51b77237f27b030567e39be58144375a053a))
* phase 3 — POSIX foreground supervisor, relaunch loop, hook, cas, statusline, dispatch ([151fb8a](https://github.com/DaveDev42/claude-smart/commit/151fb8aa34c9a1a62ca2579eb501dbc08c544cff))
* phase 4 — Windows console-control launcher (build+test verified, 2 BLOCKING checks pending manual) ([8edec0d](https://github.com/DaveDev42/claude-smart/commit/8edec0d677d67c9f54f056f716a5ed2bdc3c5cb8))
