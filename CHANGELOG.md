# Changelog

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
