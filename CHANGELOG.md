# Changelog

## [0.4.10](https://github.com/DaveDev42/claude-smart/compare/v0.4.9...v0.4.10) (2026-10-07)


### Features

* **hook:** hops.jsonl, a structured event per limit switch and model fallback ([17df3ad](https://github.com/DaveDev42/claude-smart/commit/17df3adc9500c0f6dc48bed84dcee3f12b349aa9)), closes [#34](https://github.com/DaveDev42/claude-smart/issues/34)


### Bug Fixes

* **migrate:** make the dry run apply the real run's Orca CLAUDE_CONFIG_DIR refusal ([e662637](https://github.com/DaveDev42/claude-smart/commit/e66263787026e341978e4b7ee728b1688551a83f)), closes [#41](https://github.com/DaveDev42/claude-smart/issues/41)
* **orca:** call a ticks procStart mismatch dead only when the value fits the uptime ([2d690f1](https://github.com/DaveDev42/claude-smart/commit/2d690f1d88ec38427de897df7fc1b0093de72052)), closes [#41](https://github.com/DaveDev42/claude-smart/issues/41)
* **orca:** verify a Linux session record's procStart in clock ticks — a reused pid no longer blocks retire ([7fa7703](https://github.com/DaveDev42/claude-smart/commit/7fa7703966f0224f4939ffae1867631647cc9d00)), closes [#41](https://github.com/DaveDev42/claude-smart/issues/41)
* **usage:** no age for an expiry at or before the epoch — expiresAt 0 reads as expired without "20726d ago" ([31d920c](https://github.com/DaveDev42/claude-smart/commit/31d920c76749bc2c0f91e76fc18157ade947e443))

## [0.4.9](https://github.com/DaveDev42/claude-smart/compare/v0.4.8...v0.4.9) (2026-10-06)


### Features

* **usage:** name Orca's active account in usage --json — top-level active id plus a per-row active flag ([6c93b65](https://github.com/DaveDev42/claude-smart/commit/6c93b65ff438219701c753ffaba4bb25f3eef429))

## [0.4.8](https://github.com/DaveDev42/claude-smart/compare/v0.4.7...v0.4.8) (2026-10-05)


### Features

* **accounts:** remove an orphan stash on a SQLite profile with Orca stopped ([69c9fdc](https://github.com/DaveDev42/claude-smart/commit/69c9fdcdd1afbee6d52b2eb7dae87ef1e5b4e610))
* **migrate:** import and select offline on SQLite profiles and on Linux ([13d3c0c](https://github.com/DaveDev42/claude-smart/commit/13d3c0c4f82d3f5dc35bd18a77ac89b3ad2da9ea))


### Bug Fixes

* **orca:** read the Windows Orca version from the executable's version resource ([cfbe06a](https://github.com/DaveDev42/claude-smart/commit/cfbe06ae473ae28ea6a8d72180f2c1ac54a34513))

## [0.4.7](https://github.com/DaveDev42/claude-smart/compare/v0.4.6...v0.4.7) (2026-10-05)


### Features

* **orca:** write SQLite profiles offline and read the Linux Orca version ([9d23531](https://github.com/DaveDev42/claude-smart/commit/9d235312134e822101ab9495df0549b10668c5bd))


### Bug Fixes

* **orca:** read the offline account list from profile-state.db ([e06b651](https://github.com/DaveDev42/claude-smart/commit/e06b651696b2ae1b93c97aca0386f2514c2fa28c))

## [0.4.6](https://github.com/DaveDev42/claude-smart/compare/v0.4.5...v0.4.6) (2026-10-02)


### Bug Fixes

* **idle-compact:** name the highlighted row of a two-entry slash menu from the box command's accent colour ([#48](https://github.com/DaveDev42/claude-smart/issues/48)) ([39d2b62](https://github.com/DaveDev42/claude-smart/commit/39d2b6263bdcf9b75e4b7228751887f22209a763))

## [0.4.5](https://github.com/DaveDev42/claude-smart/compare/v0.4.4...v0.4.5) (2026-10-01)


### Bug Fixes

* **idle-compact:** titled input-box rule, vim marker above agent panels, verify-failed logging ([#46](https://github.com/DaveDev42/claude-smart/issues/46)) ([4cccd6c](https://github.com/DaveDev42/claude-smart/commit/4cccd6ccf611fe93b4be506703e129f702bbcfdd))

## [0.4.4](https://github.com/DaveDev42/claude-smart/compare/v0.4.3...v0.4.4) (2026-10-01)


### Bug Fixes

* **relay:** resync pty size after installing the SIGWINCH handler ([#44](https://github.com/DaveDev42/claude-smart/issues/44)) ([80d26ce](https://github.com/DaveDev42/claude-smart/commit/80d26cec2bbcaa989fa900de5ef3c5d3d0736442))

## [0.4.3](https://github.com/DaveDev42/claude-smart/compare/v0.4.2...v0.4.3) (2026-10-01)


### Features

* **relay:** run claude in a ConPTY on Windows ([059c457](https://github.com/DaveDev42/claude-smart/commit/059c45760898af820465e41e915ecf812b3bb10e))


### Bug Fixes

* **idle-compact:** judge output quiet on screen content above the input box — a statusline countdown no longer blocks delivery ([0388f57](https://github.com/DaveDev42/claude-smart/commit/0388f576ba69187a9fa1daa0be5f6b981c12d973))
* **orca:** resolve status D like a launch and list every legacy-dir user ([236eb0b](https://github.com/DaveDev42/claude-smart/commit/236eb0b8a53d3afab32f80ea5a3ee7b3f5e9dc8d))

## [0.4.2](https://github.com/DaveDev42/claude-smart/compare/v0.4.1...v0.4.2) (2026-10-01)


### Features

* **accounts:** show each account's usage in accounts list ([20b6d84](https://github.com/DaveDev42/claude-smart/commit/20b6d8401c965d90a6aec514db51b87ac71bca8c))
* **orca:** flag an Orca that still runs the old D ([7fcff12](https://github.com/DaveDev42/claude-smart/commit/7fcff120f2c66ecf1fc3c2eb2b8228586860b38e))


### Bug Fixes

* **orca:** read a headless Orca's D when its environ is retitled ([8ebfc42](https://github.com/DaveDev42/claude-smart/commit/8ebfc42357910e8ce447fdcbb41c3776864509f7))
* **switch:** leave a capped account when no usage is known ([3f21f4b](https://github.com/DaveDev42/claude-smart/commit/3f21f4b2e54e7e291bd1fe8c5e819ddb017d74ca))
* **usage:** track the statusLine identity per config dir ([26eef6c](https://github.com/DaveDev42/claude-smart/commit/26eef6cdf35d41093cf9052e170e39fae114d8bd))

## [0.4.1](https://github.com/DaveDev42/claude-smart/compare/v0.4.0...v0.4.1) (2026-10-01)


### Features

* **idle-compact:** compact an idle session before its prompt cache expires ([affdf4b](https://github.com/DaveDev42/claude-smart/commit/affdf4b98ea99296d5ee25b5895e4b8382b6501a)), closes [#36](https://github.com/DaveDev42/claude-smart/issues/36)


### Bug Fixes

* **relay:** take the foreground in the child before exec ([2918bc4](https://github.com/DaveDev42/claude-smart/commit/2918bc4c825bf38b8ff7d3c318fcbb7e87c0cda8))

## [0.4.0](https://github.com/DaveDev42/claude-smart/compare/v0.3.8...v0.4.0) (2026-09-30)


### ⚠ BREAKING CHANGES

* **accounts:** csm profiles, the cas registry verbs, pick-account, current-usage, --profile, -A and --no-pick are removed. csm cas --print-default-dir prints D and csm cas --eval is a silent no-op so old shell shims keep loading. The first run of the new csm moves existing ~/.claude.<name> logins into Orca by itself; csm migrate shows or finishes that work.

### Features

* **accounts:** switch accounts through Orca's account store ([0fd39b5](https://github.com/DaveDev42/claude-smart/commit/0fd39b593e507bc58fa4c0b8484e04fe8aff80e8))
* **migrate:** move a machine onto Orca's account store automatically ([ea46f37](https://github.com/DaveDev42/claude-smart/commit/ea46f374f0815edd89936a8df4a9c23422c4e00a))


### Bug Fixes

* **orca:** close crash, liveness and migration gaps found in review ([0d2665c](https://github.com/DaveDev42/claude-smart/commit/0d2665ca93453c99975520ef9258e0f96f54a1ad))
* **test:** make the suite pass on native Windows and the CI e2e runners ([0ab727f](https://github.com/DaveDev42/claude-smart/commit/0ab727f310e4656f05ae8af99a54e9cb303130fc))

## [0.3.8](https://github.com/DaveDev42/claude-smart/compare/v0.3.7...v0.3.8) (2026-09-26)


### Features

* **orca:** interoperate with Orca's Claude account switching ([de91859](https://github.com/DaveDev42/claude-smart/commit/de918594decaf8a428d24ba254981b293613408b))


### Bug Fixes

* **run:** resume at once in an Orca pane instead of opening the account picker ([5d6d90a](https://github.com/DaveDev42/claude-smart/commit/5d6d90aa7b16c85a357c88ba669dada2b7cf1ab2))


### Reverts

* **orca:** drop the slot-profile Orca interop ([464f651](https://github.com/DaveDev42/claude-smart/commit/464f651d1796f7632b36d2b92af49db1669d6cf5))

## [0.3.7](https://github.com/DaveDev42/claude-smart/compare/v0.3.6...v0.3.7) (2026-09-21)


### Features

* **provision:** link every profile's sessions dir to one shared registry ([d762e1f](https://github.com/DaveDev42/claude-smart/commit/d762e1fe0df5fc1ce1387343097adc8eeb50b58a))


### Bug Fixes

* **provision:** refuse to provision a dir that fails the home floor ([b52bdbc](https://github.com/DaveDev42/claude-smart/commit/b52bdbc6d0205422773d381bd616de2d1152db80))

## [0.3.6](https://github.com/DaveDev42/claude-smart/compare/v0.3.5...v0.3.6) (2026-09-19)


### Bug Fixes

* **hook:** stop stranding a switched session on a capped Fable model ([0fb017d](https://github.com/DaveDev42/claude-smart/commit/0fb017d91021a4995834be21762d1c32ab91f0c6))

## [0.3.5](https://github.com/DaveDev42/claude-smart/compare/v0.3.4...v0.3.5) (2026-09-19)


### Features

* **cmd:** decide Windows interactivity with GetConsoleMode instead of an env guess ([c1332d6](https://github.com/DaveDev42/claude-smart/commit/c1332d625c7d3c6b2695c8a6c94e05fe58558522))
* **hook:** fall back to the latest Opus on a Fable cap instead of switching accounts ([87a6f46](https://github.com/DaveDev42/claude-smart/commit/87a6f46c13dc09bcf0a0bccf0397aaeb50487e74))


### Bug Fixes

* **cas:** refuse a CLAUDE_CONFIG_DIR outside the registry or $HOME — the floor is machine-wide ([fd0c55a](https://github.com/DaveDev42/claude-smart/commit/fd0c55ac96b576dc3248e7f5f7756fe7b18558aa))

## [0.3.4](https://github.com/DaveDev42/claude-smart/compare/v0.3.3...v0.3.4) (2026-09-18)


### Features

* **cli:** add a claude passthrough verb — csm claude &lt;args...&gt; ([66aecbd](https://github.com/DaveDev42/claude-smart/commit/66aecbd60d3be766f4dbf166253ab95d763ba9ad)), closes [#26](https://github.com/DaveDev42/claude-smart/issues/26)
* **relaunch:** carry a launch's session-shaping claude flags across a limit switch ([fc278f9](https://github.com/DaveDev42/claude-smart/commit/fc278f9a90cc186d92230f99a97ff7af219ca805))


### Bug Fixes

* **cas:** keep unit tests off the machine-wide CLAUDE_CONFIG_DIR floor — the launchctl/HKCU setters are inert under cfg(test) ([2b5a213](https://github.com/DaveDev42/claude-smart/commit/2b5a2137d7048e24bdeeef64b3d8a0be55ef5297))
* **cli:** accept a global --profile in front of a subcommand word ([37e00ce](https://github.com/DaveDev42/claude-smart/commit/37e00ce8bab56a8f4397aa4d8db9f5b837a7973a)), closes [#25](https://github.com/DaveDev42/claude-smart/issues/25)
* **run:** print run's own help instead of forwarding --help to claude ([95b2b53](https://github.com/DaveDev42/claude-smart/commit/95b2b53af39a682aaa313ea1607bee5670708cc0)), closes [#25](https://github.com/DaveDev42/claude-smart/issues/25)

## [0.3.3](https://github.com/DaveDev42/claude-smart/compare/v0.3.2...v0.3.3) (2026-09-17)


### Features

* **doctor:** keep ~/.claude as a compatibility shim — link its projects to the shared transcript dir so hardcoded-path tools see every profile's sessions ([50677f2](https://github.com/DaveDev42/claude-smart/commit/50677f2858d9c9687af7a040dc720e4a87cb54aa))
* **picker:** surface NeedsRefresh profiles in the stale-usage account picker ([47e9cca](https://github.com/DaveDev42/claude-smart/commit/47e9cca0ce225999100d345ef6d2176b182372d7))
* **provision:** self-heal the projects symlink alongside plugins — csm no longer depends on out-of-repo tooling for the shared transcript dir ([47e9cca](https://github.com/DaveDev42/claude-smart/commit/47e9cca0ce225999100d345ef6d2176b182372d7))


### Bug Fixes

* **help:** document every run flag csm actually accepts ([47e9cca](https://github.com/DaveDev42/claude-smart/commit/47e9cca0ce225999100d345ef6d2176b182372d7))
* **reaper:** normalize exe basenames through proc_check's helper ([0822bdd](https://github.com/DaveDev42/claude-smart/commit/0822bddb4d1aa31dbfe794a7a5455127879b2c02))

## [0.3.2](https://github.com/DaveDev42/claude-smart/compare/v0.3.1...v0.3.2) (2026-09-15)


### Features

* **usage:** opt-in OAuth refresh for headless collectors — gated on expired access + no live session ([#22](https://github.com/DaveDev42/claude-smart/issues/22)) ([115c711](https://github.com/DaveDev42/claude-smart/commit/115c7118fd0f7a755e211115f84e1859599f0cbb))

## [0.3.1](https://github.com/DaveDev42/claude-smart/compare/v0.3.0...v0.3.1) (2026-09-14)


### Features

* **hook:** switch off a capped account from the statusline tick — Claude Code fires no hook for a subscription cap ([21d7ef2](https://github.com/DaveDev42/claude-smart/commit/21d7ef23761e9bb26f00bfcf0bcee7b66acaf95f))
* **scoring:** weigh the model-scoped weekly cap in every pick and in the Stop hook ([da560bf](https://github.com/DaveDev42/claude-smart/commit/da560bf12cdb6c53cdc1990a9857a0090407279b))


### Bug Fixes

* **hook:** act on StopFailure(rate_limit) — the event Claude Code fires when a usage cap ends a turn ([ad2bc5c](https://github.com/DaveDev42/claude-smart/commit/ad2bc5cfc9a61d0980fb4e4b83d18061632c141a))
* **proc-check:** match claude by name, exe, or argv[0] — the limit-switch kill-gate never passed ([28c9a34](https://github.com/DaveDev42/claude-smart/commit/28c9a344d3d2f4407198792d217ed7d33ac16256))
* **relaunch:** re-apply the remembered --model/--effort/--permission-mode on a limit-switch hop ([0a88590](https://github.com/DaveDev42/claude-smart/commit/0a885904d43500691a1999fdd9a0f910e426546d))
* **run:** forward explicit --permission-mode/--effort/--model to claude — they were parsed and then dropped ([98e64a4](https://github.com/DaveDev42/claude-smart/commit/98e64a4a1126ae93dd38b1560bb3881e524763ed))

## [0.3.0](https://github.com/DaveDev42/claude-smart/compare/v0.2.15...v0.3.0) (2026-09-02)


### ⚠ BREAKING CHANGES

* **usage:** the hub fetch paths are removed. CLAUDE_USAGE_URL, CLAUDE_HUB_HOSTNAME, CLAUDE_USAGE_HTTP_TIMEOUT, and CLAUDE_USAGE_SSH_TIMEOUT no longer do anything. Usage comes only from the local cache, CSM_USAGE_CMD if set, and local per-profile collection.

### Features

* **usage:** collect per-profile usage locally — drop the hub, warn on dead credentials ([6f34045](https://github.com/DaveDev42/claude-smart/commit/6f34045e2cee8276759545a5154b2e5f44b3cb92))

## [0.2.15](https://github.com/DaveDev42/claude-smart/compare/v0.2.14...v0.2.15) (2026-08-05)


### Features

* restore -n/--new to skip the session picker ([0743a3b](https://github.com/DaveDev42/claude-smart/commit/0743a3b3496a931503e290790e73352e367b9175))

## [0.2.14](https://github.com/DaveDev42/claude-smart/compare/v0.2.13...v0.2.14) (2026-07-26)


### Bug Fixes

* **usage:** label the weekly tier column from the hub, not a constant ([2bd1085](https://github.com/DaveDev42/claude-smart/commit/2bd108581f923c4035246c96e25d6a1b90d38fdd))

## [0.2.13](https://github.com/DaveDev42/claude-smart/compare/v0.2.12...v0.2.13) (2026-07-26)


### Bug Fixes

* **usage:** track the Fable weekly cap, not the retired Sonnet one ([96f4e4c](https://github.com/DaveDev42/claude-smart/commit/96f4e4ca27918e2a76fc291119db7f0c7c5a9cab))

## [0.2.12](https://github.com/DaveDev42/claude-smart/compare/v0.2.11...v0.2.12) (2026-07-04)


### Features

* rank account pick by soonest weekly reset, split usage RESETS column ([#16](https://github.com/DaveDev42/claude-smart/issues/16)) ([5e212ab](https://github.com/DaveDev42/claude-smart/commit/5e212abe7d285e064842428c5286f91decbf7820))

## [0.2.11](https://github.com/DaveDev42/claude-smart/compare/v0.2.10...v0.2.11) (2026-06-30)


### Features

* **provision:** share plugins across profiles via shared SSOT symlink ([413252b](https://github.com/DaveDev42/claude-smart/commit/413252ba17e1b5083e5039dd2c4e851cc5426d5e))


### Bug Fixes

* **hook:** auto-switch off limited profile bypasses stale gate ([ffa1840](https://github.com/DaveDev42/claude-smart/commit/ffa1840b1ae0048ae8724212134b75fe472258e3))

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
