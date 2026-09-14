# rcli (the Wally command-line client) — Wally production launch

This document tracks what stands between `rcli` today and a public,
production Wally command-line client — the surface a stranger installs, logs
into with a Google or GitHub account, and uses to run a hosted model. It is
owned by the CLI maintainer (primary builder of the cloud/login/harness
surface), with a product owner holding naming and scope decisions and a
control-plane owner responsible for the server side of the auth handshake.
It is derived from direct inspection of the `rcli` repository at three points
in time: an earlier snapshot (detached `HEAD` at `d87502a`, PR #51 alone), the
live default branch (`origin/main` at `a88d9dd`, which adds PR #60 and PR #59
on top of PR #51), and the only published release (`v0.5.2`, tag commit
`6c3e8e9`, which predates all three). **The branch this document itself ships
on is built on `origin/main`, not on the older `d87502a` snapshot** — a fresh
clone of it already carries everything through PR #60 and PR #59. So wherever
a row below says a fix exists "only on `origin/main`, not in this checkout,"
that describes the `d87502a` snapshot specifically, not the checkout this
file lives in. The fact that does not collapse between the three points:
nothing past `v0.5.2` has shipped in a release. **The one rule for reading
it: every row is marked VERIFIED (checked directly against the repo, or
against a live `gh`/`git` command) or is flagged as an open question. Where a
claim could not be independently confirmed against this repository, it has
been removed or restated as an open question rather than left standing on
its own.**

The repo is C++20/CMake with an optional Swift host on Apple Silicon. It
already ships as a public, on-device product with 1,544 stargazers; the
cloud/login surface described here is a pivot bolted onto that existing base,
not a greenfield build.

The console, not this CLI, is the launch gate for Wally: onboarding through
the console must work on its own. This CLI may ship present and imperfect
without blocking that launch, so every blocker in this document is a bar
this CLI sets for its own production readiness, not a gate on Wally's
launch itself.

---

## 1. The ideal state

A stranger who has never heard of RunAnywhere reads three lines on the
marketing site, pastes one command into a terminal, and two minutes later is
running a coding agent against a hosted GLM model with their own account and
their own balance — the whole flow modeled explicitly on Ollama's
install-login-run shape. `curl -fsSL https://get.runanywhere.ai | bash` (or
`brew install runanywhereai/wally/wally`, or the PowerShell one-liner on
Windows) drops a signed, notarized binary — macOS, Windows x64, Windows
arm64, all three built and published by a CI/CD pipeline with nobody's laptop
in the loop. The installer offers to run `wally login` immediately unless the
shell is non-interactive or the user is already signed in, in which case it
skips straight past the prompt the way `claude login` does.

`wally login` opens the real, production console in the user's default
browser. The user picks "Continue with Google" or "Continue with GitHub" —
there is no separate account-creation step, no password, and no org-invite
gate blocking a self-serve signup. The moment they click Approve on the
console, the waiting CLI process resolves on its own: no code to copy, no
polling the user has to watch. This works identically whether the console
frontend and the console API are one deployment or two, in production or in
a developer's local sandbox — the CLI never has to guess which browser origin
it is allowed to trust, because the console tells it, unambiguously, at every
environment. That production API stays fixed at a single host,
`inference.runanywhere.ai`, through this launch.

Once signed in, `wally whoami` shows identity and the current month's spend
in one line — a single command answers "who am I and what have I used,"
rather than requiring a second command to find out the second half. `wally
usage` breaks that down by window — real numbers for "last hour" and "last
24 hours," not placeholder dashes — with `--json` for scripting and figures
that hold up correctly however large an account's spend has grown, on every
platform, including Windows, and that claim is backed by an actual human
having run it on a Windows machine, not just a green CI runner. `wally
logout` clears local state completely and revokes the server-side session;
there is exactly one way to sign in and one way to sign out, not two
competing systems a user has to learn to distinguish. `wally opencode
--cloud` (and any future coding-tool integration) launches the chosen
harness pointed at the hosted model with zero extra configuration; if the
underlying tool itself is not installed, the CLI says so in one plain
sentence with an install command, never a stack trace or a silent,
unexplained exit.

The whole cloud surface runs a security posture worth trusting with a
credit-backed account: the usage endpoint is read-only and GET-only against
the caller's own token, credentials are stored with real OS-level protection
(DPAPI on Windows, 0600 permissions elsewhere), no request ever gets logged
or echoed with the secret in it, and a header-injection or origin-spoofing
attempt against the login flow fails a real, CI-exercised test rather than
relying on hand-testing — **all four of those already hold true today, see
§2**. What does not hold yet: every artifact a user downloads is exactly
what its checksum says it is and is signed by a real, verifiable identity,
its release tag matches what actually shipped, and merging to `main` always
means a second person looked at the diff first — nobody merges their own
change and cuts a public release alone. Distribution finally either sits in
`homebrew-core` or has an explicit, permanent, and understood reason it does
not.

## 2. Where it actually is today

### Rename and release/tag pipeline

| Statement | Status |
|---|---|
| Repo is `RunanywhereAI/rcli`, default branch `main`, **already public** since 2026-03-04, 1,544 stars, 85 forks, **10 open issues** (#3, #9, #14, #20, #24, #27, #29, #30, #31, #32) | VERIFIED — `gh repo view`, `gh issue list --state open` |
| **No branch protection, no CODEOWNERS** on `main`; anyone with write access can merge and, with a `release:*` label, autonomously cut and publish a release | VERIFIED — `gh api repos/.../branches/main/protection` → 404; `find . -iname CODEOWNERS` → none |
| Merging a labelled PR runs `.github/workflows/auto-tag.yml`, which reads `project(rcli VERSION …)` from `CMakeLists.txt:2`, tags `v<version>`, and explicitly `gh workflow run`s `release.yml` (a tag push alone does not trigger it) | VERIFIED — full file read |
| `release.yml` builds three legs in parallel (`macos` on `macos-26`, `windows-arm64` on `windows-11-arm`, `windows` on `windows-2022`), packages each, verifies with `scripts/verify-release-assets.py`, then a `publish` job stamps the Homebrew formula and creates the GitHub Release | VERIFIED — full file read; `.github/workflows/release.yml:21,68,121,171` |
| **No code signing or notarization anywhere in the pipeline** — self-declared as an open launch gate | VERIFIED — `docs/RELEASING.md:65-90` ("Signing reality and production gates"), corroborated by no `codesign`/`notarytool`/`signtool` step anywhere in `release.yml` |
| **The only published release, `v0.5.2` (tagged 2026-08-27), predates all cloud/account code** — `git show v0.5.2:src/commands/cmd_account.cpp` returns "does not exist in 'v0.5.2'" | VERIFIED directly |
| `login`/`whoami`/`logout` merged to `main` in PR #51 (`d87502a`); never tagged in any release | VERIFIED — `git show d87502a:src/commands/cmd_account.cpp`; `gh release list` still shows `v0.5.2` |
| `usage` is merged to `main` in PR #60 (`6e2c144`) — present in this checkout, not only "on `origin/main`" — but absent from every release, since `v0.5.2` predates it | VERIFIED — `src/commands/cmd_usage.cpp` exists in this checkout (219 lines); `git show v0.5.2:src/commands/cmd_usage.cpp` → not found |
| 8 merged PRs sit unreleased on top of `v0.5.2` (`#52,#53,#54,#57,#58,#51,#60,#59`) | VERIFIED — `git log --oneline v0.5.2..origin/main`; `gh release list` still shows `v0.5.2` as latest |
| Homebrew: `Formula/rcli.rb` is live and correct; a second, orphaned `packaging/homebrew/rcli.rb.in` is unreferenced by any script and points at the wrong repo | VERIFIED — grep for `packaging/homebrew\|rcli.rb.in` across every script: zero hits |
| `install.sh` (macOS Apple Silicon only) and `install.ps1` (Windows) both exist and both do what they claim; see the §3 Phase 2 note on which one actually verifies a checksum before install | VERIFIED |
| `docs/RELEASING.md` and `.claude/skills/rcli-release/SKILL.md` both say the release ships **2** archives; it actually ships **3** (`windows-arm64` was added later and never back-ported to either doc) | VERIFIED — cross-checked |
| PR #61 (the rcli→wally rename) — is open, mergeable, all CI green, **0 human reviews** | VERIFIED (live) — `gh pr list`, `gh pr view 61` |
| PR #61's own body leaves `wally launch` phrasing as an explicit open question | VERIFIED — PR body via `gh pr view 61` |
| SDK kit pinned exactly: `RCLI_PINNED_SDK_VERSION "0.20.34"`, IDL `1.1.1`, protoc `35.1` | VERIFIED — `cmake/sdk-pin.cmake:15-21` |

### Device-login / browser approval flow

| Statement | Status |
|---|---|
| Through PR #51 (`d87502a`), `Login()` took exactly one URL (`--console-url`/`RCLI_CONSOLE_URL`) and required the server's `verification_url` to share its **exact origin** — no separate web-URL concept existed in the code at all. PR #60 (`6e2c144`), merged to `main` and present in this checkout, replaced that single check with the split described two rows below | VERIFIED — `git show d87502a:src/commands/cmd_account.cpp` (old check) vs `src/commands/cmd_account.cpp:179-182` (current) |
| Consequently, through PR #51: any environment where the console API and the console web app were different origins made `rcli login` **hard-fail** with "console returned an approval URL outside its origin," before a browser ever opened. That blanket failure is gone for the production origin (see the allowlist fix below); a **dev** console whose approval page differs from its API origin still hits it unless `RCLI_CONSOLE_WEB_URL` is set | VERIFIED by reading `BrowserUrlIsTrusted()`/`TrustedBrowserOrigins()` |
| `AGENTS.md` already documents the real defaults — API `https://inference.runanywhere.ai` via `RCLI_CONSOLE_URL`, approval page `https://console.runanywhere.ai` via `RCLI_CONSOLE_WEB_URL` — matching `DefaultConsoleUrl()`. The earlier wrong default ("defaults to `http://localhost:8080`") was specific to PR #51's `d87502a` snapshot and is not present in this checkout. See RCLI-29 | VERIFIED — `AGENTS.md:112-115` vs `src/account/credentials.cpp:517-519`; old text at `git show d87502a:AGENTS.md:82-83` |
| **Merged to `main` in PR #60 (`6e2c144`) and present in this checkout; absent from every release**: splits the URL into `RCLI_CONSOLE_URL` (API) and `RCLI_CONSOLE_WEB_URL` (approval page); `TrustedBrowserOrigins()` trusts an explicit `RCLI_CONSOLE_WEB_URL` if set, else a hardcoded production allowlist, else the API origin itself for any other (dev) console | VERIFIED — `src/account/credentials.cpp:60-71,522-535` |
| **A dev deployment still needs `RCLI_CONSOLE_WEB_URL` set explicitly** whenever its console and API are on different origins — there is no autodiscovery, even after the fix | VERIFIED — reading `TrustedBrowserOrigins()`'s fallback branch |
| **Even production's fix is a hardcoded allowlist, not a real solution**: the code lists the production console domain plus one other literal hostname the control plane currently hands out for the approval page, with a comment explaining the second entry should be deleted once the control plane returns the custom domain directly — "a one-line config change on the API side." The same comment records that, as measured on 2026-09-04, both hostnames currently serve the same deployment (matching `server`/`etag` headers) — today they are one deployment behind two names, not two independently-trusted origins | VERIFIED — code comment, `origin/main` `src/account/credentials.cpp:60-71` |
| Two parallel, un-unified auth systems exist: `rcli login` (device/browser) and `rcli auth login` (API-key handshake via `RUNANYWHERE_ENVIRONMENT`/`RUNANYWHERE_BASE_URL`/`RUNANYWHERE_API_KEY`) | VERIFIED — `README.md:348-350`: "This is separate from `rcli auth login`... The two are being unified" — no tracking doc/issue found anywhere in the repo |
| `--no-browser` already works, printing the code/URL and skipping `OpenBrowser()` | VERIFIED — `src/commands/cmd_account.cpp:154,186-190,310` |
| Credentials stored at `$XDG_CONFIG_HOME/rcli/credentials.json` (POSIX) or DPAPI-encrypted `%LOCALAPPDATA%\RunAnywhere\RCLI\credentials.dat` (Windows); `RCLI_PROFILE_DIR` overrides the location | VERIFIED |
| Defensive tests already exist guarding the login path against header injection and secret-echoing | VERIFIED — `tests/test_rcli_account.cpp`: `test_console_rejects_header_injection`, `test_console_errors_do_not_echo_secrets` |
| No simulate/fake/stub/mock login code path exists anywhere in `src/account/` on any checkout | VERIFIED — repo-wide grep, zero hits |
| The mock console `tests/test_account_cli.py` uses always serves the API and the browser-approval page from the same local origin, so CI **structurally cannot** exercise or catch the dev split-origin failure | VERIFIED — reading the test's mock server setup |
| Windows CI being green (`ci.yml` runs `ctest`/`e2e.sh` on `windows`/`windows-arm64`) is **not** the same claim as "a human has run `login`/`whoami`/`usage`/`logout` against real Windows hardware." Nothing found in the repository closes that gap either way — `docs/VERIFICATION.md`'s own per-release manual checklist, which is where that proof would live, is a blank template | Open question, unresolved — see RCLI-44 |

### Cloud commands: login / whoami / usage / logout

| Statement | Status |
|---|---|
| `login`, `whoami`, `logout` implemented and shipped in `d87502a` — not in any release | VERIFIED |
| `rcli usage` was added in PR #60 and is present in this checkout: two fixed windows, `"1h"`→`"past 1h"` and `"24h"`→`"past 24h"` (`kRows[]`) | VERIFIED — `src/commands/cmd_usage.cpp:68` |
| If the console sends no data for a window id, that row prints all four columns as `"-"` and a footer line explains: "this console does not total by window yet; a dash is a number it did not send, not a zero" | VERIFIED — same file, `Window()`/`kRows` loop and the status line |
| **The dash behavior is a deliberate, correct CLI-side design choice** — the actual defect is that the console backend does not yet send per-window totals. The CLI's own code says so directly, in a comment on the function that looks the window up | VERIFIED — `src/commands/cmd_usage.cpp` |
| A prior int64-overflow bug (spend counters carried as `long`, overflowing past **$2,147** on Windows) was fixed in this same PR | VERIFIED — commit message on `origin/main`: "fix(usage): carry counters as int64, which long is not on Windows past $2,147 of spend" |
| `--json` output and honoring the root `--json` flag (not a command-local one) were also fixed in this PR | VERIFIED |
| `FetchUsage()` calls `GET {origin}/v1/cli/usage?days=<1..365>` with the access token | VERIFIED — `src/account/console.cpp:753` |
| `rcli whoami`'s README one-liner promises "who you are, and what you have used this month" (`README.md:19`) — **`WhoAmI()` does not do this**: it prints only email, session status, and the console URL, never anything from usage data, and PR #60 (which added `usage` as its own command) left `WhoAmI()` untouched | VERIFIED — `src/commands/cmd_account.cpp:259-292`. See RCLI-41 |

### Coding harness

| Statement | Status |
|---|---|
| `src/harness/harness.h`/`.cpp` implement a clean, tool-agnostic abstraction (`Endpoint`, `Resolve()`, `Release()`, `Launch()`) that never learns which source (local or hosted) it got | VERIFIED — full file read |
| `rcli opencode` is the one wired harness; `--cloud` forces the hosted path with no local fallback | VERIFIED — `README.md:44-48` |
| `rcli claude-code`/`clion`/`rustrover` point a loopback OpenAI/Anthropic-translating proxy at a local-or-hosted model — a separate mechanism from the harness abstraction | VERIFIED |
| **Through PR #51 (`d87502a`), a missing target tool on macOS/Linux produced no user-facing error at all** — `Spawn()` forks and `execvp`s; on failure the child calls `_exit(127)` silently. Only the Windows path (`_spawnvp`) printed `"<tool> is not on PATH"` | VERIFIED — `git show d87502a:src/harness/harness.cpp` |
| **Fixed in PR #60 (`6e2c144`), merged to `main` and present in this checkout**: a new `OnPath()` preflight walks `$PATH` (handling Windows suffixes and the POSIX empty-component-means-cwd rule) before forking, and on a miss prints `"<tool> is not installed on this machine"` plus a per-tool install hint. This is unreleased: `v0.5.2` predates it | VERIFIED — `src/harness/harness.cpp:156-207` |
| `skills/runanywhere/SKILL.md` (PR #60, present in this checkout, unreleased) implements a "copy-paste terminal skill installs, logs in, drops into a harness" onboarding flow, and explicitly tells the assistant to check "not signed in" first rather than launching a TUI at a brand-new user | VERIFIED |
| `install.sh` (PR #60, present in this checkout, unreleased) pulls that skill file from the release tag (not `main`, so an installed assistant's instructions cannot drift under it) and runs `rcli login` automatically post-install unless non-interactive or already signed in | VERIFIED — `install.sh:88-121` |

### Error handling, tests, docs

| Statement | Status |
|---|---|
| Fixed in PR #60, merged to `main` and present in this checkout — real bugs through PR #51 (`d87502a`): the local OpenAI-compatible proxy wrote prompt text to a world-readable `/tmp` path; login rejected some legitimately base64url-encoded request codes; a cloud session/model id could be acted on before being verified | VERIFIED — `src/ide/openai_proxy.cpp:111-119` (trace path, not `/tmp`); `src/account/console.cpp:475-484` (`RequestCodeIsSafe`, full base64url alphabet); `src/harness/harness.cpp:309-320` (`VerifyCloudSession`) |
| `rcli bench --trials` did not range-check negative integers sensibly through PR #51 (`d87502a`); fixed in PR #60, present in this checkout | VERIFIED — `src/commands/cmd_bench.cpp:770-775` (`CLI::Range(1, INT_MAX)`) |
| Unit tests are hermetic by construction (`test_rcli_account.cpp`'s `ConsoleClient` takes an injectable HTTP-transport callback rather than making real requests) | VERIFIED |
| `tests/test_account_cli.py` execs the real built binary three times against a local mock console — explicitly not a pure unit test | VERIFIED |
| CI (`ci.yml`) runs `ctest` and `scripts/e2e.sh` on `macos`, `windows`, `windows-arm64`. **No Linux build or test job exists at all**, matching `AGENTS.md`'s own disclaimer | VERIFIED — full workflow file read; `AGENTS.md:177` ("Linux bottles are not a v1 merge blocker. Windows x64 and macOS arm64 are.") |
| Device/model-serving e2e scripts are gated behind `RCLI_E2E_*` env vars that public CI leaves entirely unset — those round-trips run only manually | VERIFIED |
| `docs/VERIFICATION.md`'s manual per-release checklist is committed as a **blank template** — no evidence it has ever been filled in for a real release | VERIFIED |
| PR #60 adds `tests/test_rcli_harness.cpp` (316 lines) and substantially expands `test_rcli_account.cpp`, `test_rcli_opencode.cpp`, `test_account_cli.py` — merged to `main` and present in this checkout; none of it exists in the `v0.5.2` release, which predates PR #60 | VERIFIED — `tests/test_rcli_harness.cpp` (316 lines), registered in `tests/CMakeLists.txt:22-26` |
| As of `d87502a`, tracked docs were exactly `AGENTS.md`, `CLAUDE.md` (symlink), `CONTRIBUTING.md`, `README.md`, `docs/RELEASING.md`, `docs/VERIFICATION.md`, plus 5 mirrored skill files (`.agents/skills/*` and `.claude/skills/*`) — no `LAUNCH.md`/`CHECKLIST.md`/`ROADMAP.md`. This checkout has since added two more tracked files: `skills/runanywhere/SKILL.md` (PR #60's onboarding skill) and this document itself, `docs/WALLY_LAUNCH_CHECKLIST.md` | VERIFIED — `git ls-files` |
| The only doc-shaped CI gate (`agents-sync.yml`) checks two things only (the `CLAUDE.md` symlink; skill-tree byte-parity) — no Markdown linter, no link checker | VERIFIED — full workflow read |
| A docs-only push straight to `main` (no branch protection) skips the whole build matrix via `paths-ignore: ['Formula/**', '**.md']` on the `push` trigger only | VERIFIED — `.github/workflows/ci.yml:4-9` |

### What already works (do not lose sight of this)

- The device-flow login mechanics themselves — request-code/poll-secret
  handshake, `BeginAuthorization`/poll/refresh/revoke against
  `/auth/cli/start|poll|refresh|revoke` and `GET /v1/me` — are implemented,
  tested against a fake transport, and defended against header injection and
  secret-echoing. VERIFIED.
- The harness abstraction (`Endpoint`/`Resolve`/`Release`/`Launch`) is a
  clean design that already generalizes across local and hosted sources
  without a harness needing to know which one it got. VERIFIED.
- The release pipeline itself — tag trigger, three-platform parallel build,
  archive verification, Homebrew formula stamping, GitHub Release creation —
  runs and is green today; the gap is what it ships and how it's guarded,
  not whether it runs. VERIFIED (`gh run list`: latest CI run
  `33853672444`, success).
- `install.sh`/`install.ps1` correctly detect architecture (including seeing
  through WOW64 on Windows ARM64) and fail closed with a clear message on
  unsupported platforms rather than installing something broken. VERIFIED.
  `install.ps1` in particular already fails closed on a checksum mismatch
  against the release's `.sha256` sidecar, and separately fails closed if
  the candidate `.exe` doesn't report the expected version when actually
  run — with an atomic backup/rollback of the previous install either way.
  VERIFIED — `install.ps1:62-97,113-135`.
- Windows credential storage genuinely uses DPAPI, not a plaintext file.
  VERIFIED.
- Windows CI (`ci.yml`) builds and runs `ctest`/`scripts/e2e.sh` on
  `windows`/`windows-arm64`, and it passes. **This is not the same claim as
  "verified on a real Windows machine"** — nothing found in this survey
  resolves that doubt either way. Treat Windows support as CI-verified only
  until RCLI-44 closes it.

## 3. The gap — everything remaining

Grouped into phases ordered by what blocks what: naming has to settle before
docs and marketing can be written against it; a real release has to exist
before any launch claim about the CLI is true; the auth/usage fixes already
exist unreleased and mainly need to be shipped; hardening and governance can
run in parallel with the above.

### Phase 0 — Naming and scope decisions (block everything downstream)

| ID | Item | Why it is needed | Owner | Size | Blocks | Source | State |
|---|---|---|---|---|---|---|---|
| RCLI-1 | Rule on PR #61 (rcli→wally rename): merge, reject, or send back for changes | It is an open PR with **0 human reviews** carrying the entire rename; every downstream doc/install-command/marketing reference depends on the outcome | product owner (decision), CLI maintainer (author) | M | RCLI-4, RCLI-11 | repo: PR #61 (`gh pr view 61`) | Open |
| RCLI-2 | Settle whether the CLI binary/repo/wire values become `wally` permanently, or only the cloud brand is "Wally" while the binary stays `rcli` | This has not been ratified anywhere: PR #61's own body says the GitHub repo name, the four console auth endpoints, the on-disk credential path, and the `client` wire value (still `"rcli"`) all stay `rcli` forever. The PR does soften the command-name half of this: `RCLI_*` env vars keep working as a fallback from the new `WALLY_*` names, with a deprecation warning, so the losing name already has a compatibility shim built for it | product owner | S | RCLI-1, all public-facing docs | repo: PR #61 body (`gh pr view 61`) (see §5, ruling 1) | Open |
| RCLI-3 | Decide what `wally launch` should do, or drop the phrasing | Explicitly left open inside the rename PR itself | product owner | S | RCLI-1 (clean merge) | PR #61 body (`gh pr view 61`) | Open |
| RCLI-4 | Decide whether `rcli login` (device/browser) is meant to fully replace `rcli auth login` (API key), or both stay as distinct, documented paths | README says "being unified" with zero tracking doc/issue found anywhere | control-plane owner, CLI maintainer, product owner | M | Docs/onboarding messaging | repo: `README.md:348-350` | Open |

### Phase 1 — Ship a real release (hard launch blocker)

Eight merged PRs already sit unreleased on top of `v0.5.2`, carrying the
entire cloud/login/opencode/usage surface described in this document.

| ID | Item | Why it is needed | Owner | Size | Blocks | Source | State |
|---|---|---|---|---|---|---|---|
| RCLI-5 | Cut a release (`release:*` label) that includes all 8 merged, unreleased PRs — the entire cloud/login/opencode/usage surface | No published tag has **any** account/cloud code today; public installers hand out `v0.5.2` | product owner/CLI maintainer | S (mechanical, once ready) | RCLI-6 through RCLI-10, the whole public launch claim | repo: `git log v0.5.2..origin/main` | **Not done — hard blocker** |
| RCLI-6 | Cut the release from `origin/main`'s tip (or an equivalent branch), not from an older snapshot like PR #51's `d87502a`, so the login/usage/harness fixes below actually ship together | Already true for the checkout this document ships on — `git merge-base HEAD origin/main` lands exactly on `origin/main`'s tip, so this branch already carries everything through PR #60 and PR #59. The caution is for whichever checkout actually runs the release step | CLI maintainer | S | RCLI-5 | repo: `git merge-base HEAD origin/main` = `a88d9dd` (`origin/main`'s tip); `git log HEAD..origin/main` (empty) | Done for this checkout — remains a caution for whoever cuts the release |
| RCLI-7 | Land the `RCLI_CONSOLE_WEB_URL` split-origin fix (PR #60) into the release | This is the exact, verified root cause of the CLI authorization approval flow failing whenever a console's API and web app are on different origins — merged to `main`, present in this checkout, absent from every release | CLI maintainer | (already built, needs release) | RCLI-5 | repo: `src/account/credentials.cpp:60-71,522-535` | Fixed in PR #60, present in this checkout; unreleased |
| RCLI-8 | Land `rcli usage` (PR #60) into the release | Currently doesn't exist in any checkout users can install (no release has it); this is the direct fix for `rcli usage` windows rendering as dashes being visible at all (the dash *content* is a separate, backend-side item, RCLI-9) | CLI maintainer | (already built, needs release) | RCLI-5 | repo: `src/commands/cmd_usage.cpp` | Fixed in PR #60, present in this checkout; unreleased |
| RCLI-9 | Get the console backend to ship server-side windowed usage totals (1h/24h) so `rcli usage`'s dashes become real numbers | The CLI's dash-when-missing behavior is correct by design; the backend doesn't send per-window data yet | the cloud backend team | L (cross-repo) | Usable `rcli usage` | repo: `src/commands/cmd_usage.cpp` (comment explaining the missing-window case) | Open — not an rcli-repo fix |
| RCLI-10 | Land the POSIX missing-tool fix (`OnPath()`/`InstallHint()`, PR #60) into the release | Through PR #51 (`d87502a`), on the platforms actually shipped (macOS/Linux), a missing harness tool exited silently with code 127 and zero message; PR #60 fixes this and is present in this checkout | CLI maintainer | (already built, needs release) | Onboarding UX, RCLI-5 | repo: `git show d87502a:src/harness/harness.cpp` (silent exit) vs `src/harness/harness.cpp:156-207` (fix) | Fixed in PR #60, present in this checkout; unreleased |
| RCLI-11 | Land the onboarding skill (`skills/runanywhere/SKILL.md`, PR #60) and its tag-pinned `install.sh` into the release | This is the concrete implementation of the "copy-paste terminal skill installs → login → harness" onboarding flow | CLI maintainer | (already built, needs release) | RCLI-5, onboarding claims | repo: PR #60 | Fixed in PR #60, present in this checkout; unreleased |
| RCLI-12 | Get the control plane's approval-page response to return the custom console domain instead of a separate infrastructure hostname | The CLI-side allowlist fix explicitly says it is waiting on this one-line API-side change; today's mitigating fact is that both hostnames currently resolve to the same deployment (matching `server`/`etag` headers, measured 2026-09-04), so this is latent risk, not a live outage — but it turns live the moment the two diverge | the cloud backend team | S (cross-repo) | Removing a fragile hardcoded allowlist entry | repo: `src/account/credentials.cpp:60-71` (origin/main comment) | Open — not an rcli-repo fix |
| RCLI-44 | Run one real `login`/`whoami`/`usage`/`logout` round trip on physical Windows hardware before the launch claims Windows works | Windows CI is green, but nothing in the repository — including the per-release `docs/VERIFICATION.md` checklist, which is a blank template — shows a human has ever exercised the cloud commands on an actual Windows machine | CLI maintainer (or whoever has Windows hardware) | S | Confidence in the "every platform, including Windows" launch claim | repo: `docs/VERIFICATION.md` | Open |

### Phase 2 — Release-pipeline hardening (launch-gate items, can run alongside Phase 1)

| ID | Item | Why it is needed | Owner | Size | Blocks | Source | State |
|---|---|---|---|---|---|---|---|
| RCLI-13 | Add macOS code signing + notarization to `release.yml` | Self-declared open launch gate; every public macOS download today is unsigned/ad-hoc-signed | whoever owns release infra | L | Public launch trust/Gatekeeper | repo: `docs/RELEASING.md:65-90` | **Not done — hard blocker** |
| RCLI-14 | Add Windows Authenticode signing to `release.yml` | Same gate, Windows side; unsigned `.exe`/DLLs trigger SmartScreen warnings | whoever owns release infra | L | Public launch trust | repo: `docs/RELEASING.md:73-90` | **Not done — hard blocker** |
| RCLI-15 | Add branch protection + CODEOWNERS on `main` | Anyone with write access can merge and auto-release with zero review today | maintainers (governance) | S | Safe autonomous releases | repo: `gh api` 404; no CODEOWNERS found | Open |
| RCLI-16 | Reconcile the full Swift dependency chain (not just the pinned C++ kit) across `cmake/sdk-pin.cmake`, `ci.yml`, `release.yml`, `swift/Package.swift`, `swift/Package.resolved` | A version drift in any one of these breaks the Apple build without the pin mechanism catching it | CLI maintainer | M | Reliable Apple releases | repo: `cmake/sdk-pin.cmake:15-21` | Open |
| RCLI-17 | Make packaging verify the packaged binary's reported version matches the requested archive version | Current scripts only run `<binary> version` without diffing it against the archive tag being built | CLI maintainer | S | Catching a stale-binary-in-fresh-archive bug before it ships | repo: `scripts/package-rcli.sh` (equivalent to `scripts/package-wally.sh:128-135` on the rename branch) | Open |
| RCLI-18 | Audit macOS packaged dependencies with `otool -L`; the packager today explicitly copies only ONNX runtime libraries | Any other dynamically-linked dependency could be silently missing from the shipped bundle | CLI maintainer | M | macOS release correctness | repo: `scripts/package-rcli.sh` (rename-branch equivalent `scripts/package-wally.sh:71-90`) | Open |
| RCLI-19 | Sign nested macOS bundle code, not only the top-level dylibs and executable | Codesign of a bundle that contains its own signed sub-bundles needs each layer signed, or Gatekeeper can still reject it | CLI maintainer | M | RCLI-13 | repo: `scripts/package-rcli.sh` (rename-branch equivalent `scripts/package-wally.sh:100-119`) | Open |
| RCLI-20 | Validate `cmake --install` separately from the packaging scripts; on Apple it installs the `rcli-cxx` compile artifact, not necessarily the shipping Swift host or its Metal bundle | A CMake-level install target that silently diverges from what packaging actually ships is a release footgun | CLI maintainer | M | Release correctness | repo: `CMakeLists.txt:168-212` | Open |
| RCLI-21 | Give both installers the archive-structure/zip-slip safety `scripts/verify-release-assets.py` gives the build, and fix `install.sh`'s total lack of its own checksum verification | **`install.ps1` already fails closed on a checksum mismatch and on a running-candidate-version mismatch, with atomic backup/rollback** (`install.ps1:62-97,113-135`) — it is the stronger of the two installers, not the weaker one. `install.sh` (macOS) does **no** checksum verification of its own at all (`grep -n "checksum\|sha256" install.sh` → zero hits) and relies entirely on Homebrew's own SHA-256 check. Neither installer runs `verify-release-assets.py`: that script is a **build-time CI gate** on the archives before upload (`release.yml:60,110,158,186-192`), not an install-time step on either platform, so its zip-slip/oversized-archive/missing-member protections never apply to what a user's machine actually downloads and unpacks | CLI maintainer | S | Windows/macOS install integrity | repo: `install.ps1:62-135`; `install.sh` (grep, zero hits); `scripts/verify-release-assets.py`; `.github/workflows/release.yml:60,110,158,186-192` | Open |
| RCLI-22 | Add PowerShell linting and an actual package/install execution step to CI | `ci.yml`'s `distribution` job lints Python, Bash, and Ruby, but not PowerShell, and never runs a real install | CLI maintainer | M | Catching Windows packaging regressions before release | repo: `.github/workflows/ci.yml:20-37` | Open |
| RCLI-23 | Fix the doc mismatch: `docs/RELEASING.md` / `.claude/skills/rcli-release/SKILL.md` both say "2 platform archives"; the real ship is 3 | Two independent docs are behind the actual `release.yml` | doc owner | S | Correctness for whoever runs the release checklist | repo: cross-checked, `docs/RELEASING.md:47-56` vs `.github/workflows/release.yml:68-124` | Open |
| RCLI-24 | Delete or fix the orphaned `packaging/homebrew/rcli.rb.in` template | Points at the wrong repo (`runanywhere-sdks`) and an unpublished Linux asset; would 404 if anyone ever wired it back up believing it canonical | whoever cleans up packaging | S | Landmine removal | repo: grep-verified unreferenced | Open |
| RCLI-25 | Decide the canonical Homebrew tap repo (`RunanywhereAI/rcli` vs a separate `homebrew-tap`) | Explicit open decision in `docs/RELEASING.md`; `RCLI_TAP_REPO` must be passed manually until resolved | CLI maintainer/control-plane owner | S | Public install instructions | repo: `docs/RELEASING.md:102-106`; `scripts/update-tap.sh:57` | Open |
| RCLI-27 | Update the stale GitHub repo description/topics ("no cloud required, on-device only") | Public-facing mismatch at the exact repo a launch will point people to; current live description reads "Talk to your Mac, query your docs, no cloud required. On-device voice AI + RAG" | marketing owner or repo maintainer | S | Launch-day polish | repo: `gh repo view` | Open |

### Phase 3 — Non-blocking correctness and documentation debt

| ID | Item | Why it is needed | Owner | Size | Blocks | Source | State |
|---|---|---|---|---|---|---|---|
| RCLI-29 | Correct `AGENTS.md`'s wrong `RCLI_CONSOLE_URL` default | Already done: this checkout's `AGENTS.md:112-115` documents the real defaults (API `https://inference.runanywhere.ai` via `RCLI_CONSOLE_URL`, approval page `https://console.runanywhere.ai` via `RCLI_CONSOLE_WEB_URL`), matching `DefaultConsoleUrl()`. The wrong `http://localhost:8080` text was specific to PR #51's `d87502a` snapshot and is not on `main` | — | S | Docs accuracy | repo: `AGENTS.md:112-115` vs `src/account/credentials.cpp:517-519`; old text at `git show d87502a:AGENTS.md:82-83` | Done — already correct on `main` and in this checkout |
| RCLI-30 | Triage all **10** open GitHub issues (#3, #9, #14, #20, #24, #27, #29, #30, #31, #32) against current code | Filed 2026-03-05 through 2026-07-14, all pre-dating the cloud pivot, but none confirmed stale without a fresh look | repo maintainer | M | Calling the issue tracker launch-clean | repo: `gh issue list --state open` | Open |
| RCLI-31 | Map server error codes to client-side codes/UX in `rcli` | Needed once more than one harness is supported; only `opencode` exists today so currently unexercised | CLI maintainers | M | Future harness expansion | repo: harness section above (only `opencode` wired) | Not yet needed |
| RCLI-36 | Build a generic "one-shell-script-per-IDE-integration" architecture (`rcli vscode`, `rcli intellij`, `rcli xcode`) | Only `claude-code`/`clion`/`rustrover`/`opencode` exist today, each hand-built rather than derived from a shared template | CLI maintainer | L | Not launch-blocking for a coding-harness-only launch | repo: coding-harness section above | Open |
| RCLI-37 | Decide whether a Linux release ships, or update docs to stop implying it might | `AGENTS.md` calls Linux bottles "not a v1 merge blocker" (implying a later version), while `install.sh` deliberately fails closed on Linux today and no Linux CI job exists | CLI maintainer | L (if pursued) | Nothing today | repo: `AGENTS.md:177` vs `ci.yml` (no linux job), `install.sh` | See §5, ruling 4 |
| RCLI-40 | Rule on PR #56 (`rcli ocr`) — its own title says "DO NOT MERGE YET" because the shipped catalog model version cannot read a page | Not currently on `main`; only relevant if OCR gets advertised at launch | whoever owns the catalog pin | M | N/A unless OCR is promised | repo: PR #56 title/body (`gh pr view 56`) | Open, self-blocked |
| RCLI-41 | Make `wally whoami` show identity **and** the current month's spend, or stop advertising that it does | README describes `whoami` as showing "who you are, and what you have used this month" (`README.md:19`); `WhoAmI()` prints only email, session status, and console URL — PR #60 added `usage` as a separate command and never touched `WhoAmI()` at all | CLI maintainer | S | README accuracy; the whoami half of §1's ideal state | repo: `src/commands/cmd_account.cpp:259-292`; `README.md:19` | Open |
| RCLI-45 | Give `HttpResponse` a headers field and read/honor `Retry-After` on a 429 everywhere the CLI calls the hosted API, then forward it through both coding-harness proxies | The hosted API answers overload with HTTP 429 and a `Retry-After` header, and a client is expected to back off on it. `HttpResponse` (`src/account/console.h:19-22`) carries only `status` and `body` — no headers at all — so `login`/`whoami`/`usage`/`logout` cannot see one even when the console sends it, and `HttpError()` (`src/account/console.cpp:387-395`) prints a flat "failed with HTTP 429" regardless. The Claude-Code-facing proxy already maps an upstream rate-limit error to 429 (`src/anthropic/messages.cpp:84`) and the OpenAI-facing proxy passes a real upstream status straight through (`src/ide/openai_proxy.cpp:449-455`), but neither forwards any header, so the wrapped tool's own backoff logic never sees `Retry-After` either | CLI maintainer | M | Correct client-side backoff under overload | repo: `src/account/console.h:19-22`; `src/account/console.cpp:387-395`; `src/anthropic/messages.cpp:84`; `src/ide/openai_proxy.cpp:449-455` | Open |
| RCLI-46 | Bring `src/account/console.cpp`'s hand-built auth/account/usage requests into line with `AGENTS.md`'s own contract-first HTTP rule | `AGENTS.md`'s "P0 contract-first network rule" requires a pinned OpenAPI 3.1 artifact and a generated (or mechanically verified) binding for any HTTP call RCLI directly owns, and forbids "invent[ing] JSON with string-built paths, ad-hoc objects... unchecked response parsing." `console.cpp` builds every request this way today: a hand-built `Json` literal for `/auth/cli/start` and a string-concatenated query path for `/v1/cli/usage` | CLI maintainer | M | Compliance with the repo's own merge rule for any console HTTP call | repo: `AGENTS.md` ("P0 contract-first network rule"); `src/account/console.cpp:545` (`Json payload = {{"hostname", hostname}, {"client", "rcli"}}`), `:753` (`url = origin + "/v1/cli/usage?days=" + ...`) | Open |
| RCLI-47 | Authenticate the loopback coding-harness proxies against their own local caller, not only against the upstream console | Both `src/ide/openai_proxy.cpp` and `src/anthropic/messages.cpp` bind only to `127.0.0.1` — the risk is local, not network-wide — but neither checks an incoming `Authorization` header from the wrapped tool. The Anthropic path hands the wrapped tool a fixed literal `"rcli-local"` as `ANTHROPIC_AUTH_TOKEN` rather than a per-session secret, and the server never verifies it; the OpenAI-compatible path's `Proxy` struct carries no local-auth field at all. Any other process running as the same local user can reach the ephemeral port and spend the signed-in user's hosted credits | CLI maintainer | M | Local-privilege boundary for `opencode --cloud`, `claude-code`, `clion`, `rustrover` | repo: `src/anthropic/messages.cpp:286,300` (loopback bind; fixed token); `src/ide/openai_proxy.h:22-25` (`Proxy` has no token field); `src/ide/openai_proxy.cpp:466-467` (loopback bind) | Open |

## 4. Edge cases and failure modes

| Scenario | What should happen | What happens today |
|---|---|---|
| **Dev/prod console origin mismatch** — console API and browser-approval page on different hosts | The CLI should discover and trust the right origin automatically, or fail with a message that tells the operator exactly which env var to set | Through PR #51 (`d87502a`): hard, unconditional fail — "console returned an approval URL outside its origin" — for every environment. In this checkout (PR #60, unreleased): the production origin is now trusted via an allowlist; a **dev** console still fails the same way unless the operator already knows to set `RCLI_CONSOLE_WEB_URL` — there is still no autodiscovery |
| **A hostname the console currently hands out for the approval page changes** | Sign-in should keep working through any purely infrastructural redeploy | Production trust is a **hardcoded two-string allowlist** (the custom console domain plus one other literal hostname). If that second hostname changes, production sign-in breaks until someone edits `credentials.cpp` and cuts a new release — this is present in this checkout, not just "on `origin/main`," and it is also not released yet. The code comment's own 2026-09-04 measurement is a mitigating fact for right now: both hostnames currently serve the same deployment, meaning they are one deployment behind two names today, not two independently-trusted origins — the risk is real but latent until that changes |
| **`rcli usage` window has no server data** | A user should be able to tell "no data was sent" from "usage is genuinely zero" | Handled correctly by design: all four columns render `-` plus one footer line explaining why — but only one line distinguishes the two cases, easy to miss in a scrolling terminal, and it doesn't exist in any released build yet |
| **Usage spend exceeds ~$2,147 on Windows** | Numbers should render correctly at any spend level | Fixed in PR #60 (moved to `int64` counters), present in this checkout; the bug (32-bit `long` overflow) was real only through PR #51 (`d87502a`) and in `v0.5.2`, where `usage` does not exist at all |
| **Missing coding-tool binary at harness launch time** | A clear, actionable message (an install command, then "run this again") | On Windows: message printed. On **macOS/Linux — the platforms this product actually ships on**: through PR #51, `execvp` failed and the child called `_exit(127)` with zero output anywhere. Fixed in PR #60 via a `PATH`-walking preflight, present in this checkout, unreleased |
| **Tool removed from `PATH` between the preflight check and the actual `fork`/`exec`** | Same clear message, even in the race window | Not handled — `OnPath()` (the PR #60 fix) is a check-then-use pattern with an inherent, if narrow, gap |
| **Local OpenAI-compatible proxy on a shared/multi-user machine** | Prompt text should never be readable by another local user | Through PR #51: prompts were written to a **world-readable `/tmp` path** — a real local information-disclosure risk. Fixed in PR #60 (0600 permissions under the credential profile directory, no longer under `/tmp`), present in this checkout, unreleased |
| **A CI test standing in for the Windows credential store** | The test should exercise the real DPAPI-encrypted read/write path | Through PR #51's test suite: a test hand-wrote a **plaintext** session file to stand in for the store, so the encrypted path itself could be broken while this test still passed. Fixed in PR #60 (commit `0830713`), present in this checkout: the current Windows test instead asserts that a file which cannot decrypt must fail to load, unreleased |
| **A future rename or new subcommand, after PR #61 merges** | Adding or renaming a subcommand should not be able to crash the binary | PR #61's own first commit introduced a hardcoded, unguarded name→help-group map that crashed the Windows binary outright (`0xC0000409`) the first time a name didn't match a real subcommand; the follow-up commit wraps it in try/catch and logs-and-skips rather than deriving the map from the actually-registered subcommands — a name that happens to collide with a different real subcommand would neither crash nor be caught by anything |
| **A docs-only edit pushed straight to `main`** (no branch protection) | Should still get some CI signal before going live | `ci.yml`'s `push` trigger (not its `pull_request` trigger) has `paths-ignore: ['Formula/**', '**.md']` — a direct docs push builds and tests nothing |
| **Any PR that never touches a real backend or device** | Should still prove login/harness/model round-trips work before merge | `RCLI_E2E_*` knobs are unset everywhere in public CI; that verification is manual, and `docs/VERIFICATION.md`'s own checklist template has never been filled in for any recorded release |
| **A copy of the binary distributed without its MLX bundle sitting beside it** | MLX backends should either work or say clearly why not | MLX resolves its Metal shaders from a bundle that must sit next to the executable, per the packaging scripts' own comments; moving just the binary risks the backend silently failing to register with no error message |
| **A user installs on Windows and expects the cloud commands to behave exactly as on macOS/Linux** | Login/whoami/usage/logout should be equally trustworthy on every shipped platform | CI proves the binary builds and its unit/e2e suite passes on Windows; nothing found in this survey proves the cloud round trip has ever run on a real Windows machine. See RCLI-44 |

## 5. Open questions

Restated as plain product questions, keeping only the ones where the
question itself — and the tension behind it — can be shown from this
repository's own code, docs, or pull requests.

| # | The question | What the repo shows | Why it cannot be decided by engineering |
|---|---|---|---|
| 1 | Is the CLI itself renamed to `wally`, or does it stay `rcli` while only the cloud brand is called "Wally"? | PR #61 renames the entire repo's binary, tests, and Homebrew formula to `wally`, yet its own body says the GitHub repo name, the four console auth endpoints, the on-disk credential path, and the `client` wire value stay `rcli` permanently — an inconsistency the PR itself does not resolve | This is a product-naming and public-identity decision with consequences for URLs, install commands, and an existing 1,544-star public repo that only the product owner can settle |
| 2 | Should `wally launch` exist as a subcommand, and if so what does it do? | PR #61's own body: "raised as wanted phrasing; not implemented here, needs a decision" | It's a pure product-behavior decision with no code precedent to infer from |
| 3 | Is `rcli login` supposed to replace `rcli auth login`, or do both stay permanently as distinct flows? | README says "being unified"; no tracking doc, issue, or design artifact for that unification exists anywhere in the repo | Requires a joint auth-architecture decision between the CLI team and the control-plane team, not something either can settle unilaterally in code |
| 4 | Is Linux support a planned future item, or not actually planned despite the docs' phrasing? | `AGENTS.md` says Linux bottles are "not a v1 merge blocker" (implying a later version is expected), yet `install.sh` deliberately fails closed on Linux today, no Linux CI job exists, and no Linux-specific plan appears anywhere in the repo | Whether Linux was ever really committed to, or the phrasing just means "not yet scoped," is a product-roadmap call, not something the code can settle |

## 6. Related work

A matching internal checklist exists for the launch work that lives outside
this repository — the cloud backend's windowed usage totals and
console-origin handling, and the console frontend's own side of the
device-approval page and account unification question in ruling 3 above.
None of that work is this repository's to resolve; it is referenced here
only where it blocks an item in §3.
