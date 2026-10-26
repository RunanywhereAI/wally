# PR #219 – Comment Triage

- Repo: RunanywhereAI/wally
- PR Title: Add wally shannon, deepsec and vvah: three hosted security harnesses
- PR URL: https://github.com/RunanywhereAI/wally/pull/219
- Total comments (including replies): 66 (64 review comments + 2 PR conversation comments)
- Fetched with `gh api ... --paginate`; counts match the PR (`review_comments: 64`, `comments: 2`).
- Fix commit: `b7c5c5d` on `feat/wally-shannon`. No GitHub issues were opened (explicit instruction); design items are documented here and flagged on the PR.
- Scores: LUS = Legitimacy & Urgency (1-5), CS = Complexity (1-5).

## PR Description

The single 0.8.14 release of the hosted security-audit harnesses. Folds in #223 (deepsec + vvah) on top of shannon.

Three harnesses, one pattern: each resolves the model from the signed-in account (or `WALLY_CLOUD_KEY`), writes the tool's own config pointed at `<console>/v1`, sets the key in the child's environment only, and otherwise passes the tool's own arguments through.

| Command | Tool | Wiring |
|---|---|---|
| `wally shannon` | Keygraph Shannon `3.4.0` (npx/Docker) | Pi `models.json`, `SHANNON_AI_MODEL=runanywhere:<id>`, `SHANNON_AI_API_KEY` |
| `wally deepsec` | DeepSec `2.3.10` (npx) | Pi `models.json`, `--ai-provider runanywhere --ai-base-url <base> --ai-api-key-env WALLY_DEEPSEC_API_KEY` |
| `wally vvah` | Visa VVAH `1.5.1` (pip venv) | `config.local.yaml` roles `{via: openai}`, `OPENAI_BASE_URL` / `OPENAI_API_KEY` |

## End-to-end test (dev, signed in as a granted account)
- **Unit:** 614 lib tests pass, 53 of them harness-specific (shannon/deepsec/vvah).
- **deepsec handoff:** resolves `deepseek-v4.1-flash-1017-uncensored` through `.../api-dev/v1`, writes the Pi `models.json` (openai-completions), passes the custom-route flags and `WALLY_DEEPSEC_API_KEY`.
- **vvah handoff:** writes `config.local.yaml` mapping every role to the model `{via: openai}`, sets `OPENAI_BASE_URL=.../api-dev/v1` and `OPENAI_API_KEY`.
- **shannon:** a real Shannon scan launched through Wally reached Recon, calling the model.
- **Model call:** the endpoint all three point at serves the model with the account's bearer (200, real completion).

🤖 Generated with [Claude Code](https://claude.com/claude-code)

---

## Section 0 – Already addressed by the author before this triage (commit b2912b1)

Five CodeRabbit threads, each with the author's reply and CodeRabbit's confirmation. Marked Fixed; not redone.

### Document that Wally overrides SHANNON_AI_BASE_URL

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4369066245
- Author: coderabbitai[bot]
- File / location: `docs/EDITORS.md:357`
- Replies: [4379508292](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379508292) (sanchitmonga22), [4379698209](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379698209) (coderabbitai[bot])

**Original Comment:**
> ** Minor** · **Functional Correctness** · **Quick win** **Document that Wally overrides the base URL.** `ChildEnv::for_start` sets `SHANNON_AI_BASE_URL` to Wally’s endpoint after removing competing variables. This text says Wally drops `SHANNON_AI_BASE_URL`, which is inaccurate. ✅ Addressed in commits b2912b1 to 549055c

Status: **Fixed** (b2912b1, confirmed by CodeRabbit in the thread)

### Pi models.json field mapping and authHeader (author kept authHeader; CodeRabbit verified against Pi 0.84.4 and withdrew the removal request)

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4369066254
- Author: coderabbitai[bot]
- File / location: `docs/EDITORS.md:388`
- Replies: [4379509782](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379509782) (sanchitmonga22), [4379748161](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379748161) (coderabbitai[bot])

**Original Comment:**
> ** Minor** · **Data Integrity & Integration** · **Quick win** **Remove the unused `authHeader` field and document the model-field mapping.** The Shannon 3.4.0 Pi client uses `apiKey`, `baseUrl`, and `headers`; it does not consume `authHeader`. The example should not present that field as required. Clarify how RunAnywhere `/v1/models` fields map to Pi fields. ✅ Addressed in commits b2912b1 to 54905 ...

Status: **Fixed** (b2912b1, confirmed by CodeRabbit in the thread)

### Shell-safe Cloud-key placeholder

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4369066272
- Author: coderabbitai[bot]
- File / location: `docs/EDITORS.md:391`
- Replies: [4379508639](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379508639) (sanchitmonga22), [4379697695](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379697695) (coderabbitai[bot])

**Original Comment:**
> ** Minor** · **Functional Correctness** · **Quick win** **Use a shell-safe Cloud-key placeholder.** `export SHANNON_AI_API_KEY=` treats `` as shell redirection operators. Copying this command can fail with a shell syntax error. ✅ Addressed in commits b2912b1 to 549055c

Status: **Fixed** (b2912b1, confirmed by CodeRabbit in the thread)

### Node.js requirement conditional on npx fallback (README)

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4369066278
- Author: coderabbitai[bot]
- File / location: `README.md:67`
- Replies: [4379508964](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379508964) (sanchitmonga22), [4379713981](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379713981) (coderabbitai[bot])

**Original Comment:**
> ** Minor** · **Functional Correctness** · **Quick win** **Make the Node.js requirement conditional.** Node 18+ is required only when Wally falls back to `npx`. Users with Shannon 3.4.0 installed on `PATH` do not need Node.js. ✅ Addressed in commits b2912b1 to 549055c

Status: **Fixed** (b2912b1, confirmed by CodeRabbit in the thread)

### Shell-safe placeholders in the SKILL.md Shannon command

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4369066286
- Author: coderabbitai[bot]
- File / location: `skills/runanywhere/SKILL.md:74`
- Replies: [4379509364](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379509364) (sanchitmonga22), [4379581601](https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379581601) (coderabbitai[bot])

**Original Comment:**
> ** Minor** · **Functional Correctness** · **Quick win** **Use shell-safe placeholders in the Shannon command.** `` and `` use shell redirection characters. Copying the command can fail. ✅ Confirmed as addressed by @sanchitmonga22

Status: **Fixed** (b2912b1, confirmed by CodeRabbit in the thread)

## Section 1 – Quick & Easy Fixes

All fixed in `b7c5c5d` unless stated. Entries group comments that raise the same root issue.

### QEF-1 – Shannon env-vars table row omits SHANNON_AI_BASE_URL

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648416  (author: cubic-dev-ai[bot], `docs/EDITORS.md:34`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comment:**
> [4379648416] P3: The `shannon` row lists only `SHANNON_AI_MODEL` / `SHANNON_AI_API_KEY`, but wally also sets `SHANNON_AI_BASE_URL` for the child. Add it to the list.

**Plan / Notes:** Added SHANNON_AI_BASE_URL to the `shannon` row of the command table in docs/EDITORS.md. Verified: `ChildEnv::for_start` sets it (shannon.rs).

Status: **Fixed** (b7c5c5d)

### QEF-2 – The VVAH report example passes -m, which report ignores

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628048  (author: cubic-dev-ai[bot], `docs/EDITORS.md:494`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comment:**
> [4379628048] P2: This example is misleading: `-m` is ignored for `report`. Only `scan`, `remediate` and `doctor` use the model. Drop `-m` here, or show it with `scan`.

**Plan / Notes:** Verified in vvah.rs: only scan, remediate and doctor are routed. The example now shows `-m` with `scan --repo`.

Status: **Fixed** (b7c5c5d)

### QEF-3 – VVAH section omits the variables removed from the child

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628148  (author: cubic-dev-ai[bot], `docs/EDITORS.md:500`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comment:**
> [4379628148] P3: This sentence omits the removal step. VVAH also strips `ANTHROPIC_*` and `WALLY_CLOUD_KEY` from the child environment, as the Shannon and DeepSec sections do. Document that here so the three sections match.

**Plan / Notes:** Verified OVERRIDING_VARIABLES in vvah.rs (ANTHROPIC_API_KEY, ANTHROPIC_SDK_API_KEY, ANTHROPIC_AUTH_TOKEN, WALLY_CLOUD_KEY). Documented all four.

Status: **Fixed** (b7c5c5d)

### QEF-4 – 0600 config claim is Unix-only, and the config may be staged in temp

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379724802  (author: cubic-dev-ai[bot], `docs/EDITORS.md:497`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379724786  (author: cubic-dev-ai[bot], `docs/EDITORS.md:497`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comments:**
> [4379724802] P3: The 0600/owner-only guarantee is enforced only on Unix; on Windows the file inherits its directory ACL. Qualify the claim for Windows or enforce a restrictive ACL there.
> [4379724786] P3: The config is not always written under the profile: Wally relocates it to a system-temp directory when the profile path is inside the scan target. Document this fallback so users know where the file may be staged.

**Plan / Notes:** Verified `write_private_file` sets mode 0600 only under cfg(unix), and `config_dir_for_target` relocates the config to the system temp dir. Both documented in the VVAH section. (Enforcing a Windows ACL was not attempted: that is a code change on a platform this worktree cannot test.)

Status: **Fixed** (b7c5c5d)

### QEF-5 – A python3 newer than 3.14 gets a different message than missing/too old

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642487  (author: cubic-dev-ai[bot], `docs/EDITORS.md:504`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648371  (author: cubic-dev-ai[bot], `docs/EDITORS.md:505`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comments:**
> [4379642487] P3: This paragraph describes only the missing/too-old message. A `python3` newer than 3.14, which the paragraph above calls common, instead gets `VVAH needs Python 3.11–3.14 (python3 is 3.15). Install Python 3.12, then run this again.` Add that case so the reader can match the output.
> [4379648371] P3: A missing or old `python3` does not necessarily stop installation because VVAH also searches versioned interpreters, and Python 3.15 gets a different error. Describe the failure as no supported interpreter being found.

**Plan / Notes:** Verified `too_new_python_message` and that candidates include versioned interpreters. The VVAH section now describes both messages and says no supported interpreter, not just python3.

Status: **Fixed** (b7c5c5d)

### QEF-6 – Nested backticks in the install prompt break the code span (MD038)

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379691343  (author: coderabbitai[bot], `docs/EDITORS.md:473`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379724813  (author: cubic-dev-ai[bot], `docs/EDITORS.md:472`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comments:**
> [4379691343] ** Minor** · **Maintainability & Code Quality** · **Quick win** **Fix the nested Markdown code span.** The installation prompt uses backticks inside a single-backtick code span. Markdown can render this text incorrectly and trigger MD038. Use double backticks around the full prompt or remove the inner backticks. _Source: Linters/SAST tools_
> [4379724813] P3: Wrap the full installation prompt in double backticks so the inner backticks do not split the code span and trigger MD038.

**Plan / Notes:** Wrapped the prompt in a double-backtick code span in docs/EDITORS.md.

Status: **Fixed** (b7c5c5d)

### QEF-7 – SKILL.md DeepSec prerequisite is too loose about PATH

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379724661  (author: cubic-dev-ai[bot], `skills/runanywhere/SKILL.md:79`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comment:**
> [4379724661] P2: The DeepSec prerequisite is stated too loosely. Wally uses a `deepsec` on PATH only when it reports exactly 2.3.10, so an agent could tell a user with another version that Node is not needed when it is. Name the exact version.

**Plan / Notes:** Verified select_runner uses an installed deepsec only when it reports exactly 2.3.10. SKILL.md now says so.

Status: **Fixed** (b7c5c5d)

### QEF-8 – 'Needs approval' wording asserts more than the check shows

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628137  (author: cubic-dev-ai[bot], `skills/runanywhere/SKILL.md:132`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648384  (author: cubic-dev-ai[bot], `docs/EDITORS.md:411`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comments:**
> [4379628137] P3: The error text says the model needs approval only 'if' it does. Reword the bullet to say the model may need approval, and to check the model name first.
> [4379648384] P3: Absence from the account list can also mean a typo or nonexistent model ID; the check does not establish that approval is needed. Tell users to verify the ID and request access only if the model is gated.

**Plan / Notes:** A model missing from the list can also be a typo. SKILL.md and docs/EDITORS.md now say it may need approval and to check the id first. The runtime message in shannon.rs/hosted.rs was left as is (not raised as a defect).

Status: **Fixed** (b7c5c5d)

### QEF-9 – SKILL.md says routing is never on the command line, but DeepSec's is

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628037  (author: cubic-dev-ai[bot], `skills/runanywhere/SKILL.md:81`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comment:**
> [4379628037] P2: DeepSec routing is injected as command-line flags, and VVAH routing is written to a config file; only credentials stay off argv. Describe routing as tool-specific flags/config and reserve “never on the command line” for keys.

**Plan / Notes:** Verified DeepSec routing is injected flags and VVAH's is a config file. SKILL.md now describes routing per tool and reserves 'not on the command line' for keys.

Status: **Fixed** (b7c5c5d)

### QEF-10 – mod.rs module doc is inaccurate

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642474  (author: cubic-dev-ai[bot], `src/harness/mod.rs:2`)
- LUS: 3  |  CS: 1  |  Type: docs

**Original Comment:**
> [4379642474] P3: The updated module doc is inaccurate and reads as a broken sentence. Shannon, DeepSec, and VVAH only run against hosted account models, and the sentence omits DeepSec and VVAH. Reword it to name all three hosted security harnesses and drop the 'hosted or local' phrasing for them.

**Plan / Notes:** Reworded: the coding tools run against hosted or local models; Shannon, DeepSec and VVAH run only against hosted account models.

Status: **Fixed** (b7c5c5d)

### QEF-11 – Manual Shannon recipe inherits provider keys and Pi login

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648320  (author: cubic-dev-ai[bot], `docs/EDITORS.md:394`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comment:**
> [4379648320] P2: If a user already has provider keys or Pi auth enabled, this direct-use recipe inherits them; Shannon may use them instead of `SHANNON_AI_API_KEY`. Clear those overrides and set `SHANNON_USE_PI_AUTH=0` before launching.

**Plan / Notes:** Verified the launcher removes OPENAI/ANTHROPIC/XAI keys and sets SHANNON_USE_PI_AUTH=0. The 'Without wally' recipe now does the same.

Status: **Fixed** (b7c5c5d)

### QEF-12 – Golden README counts are stale

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628107  (author: cubic-dev-ai[bot], `tests/golden/README.md:122`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642453  (author: cubic-dev-ai[bot], `tests/golden/README.md:121`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comments:**
> [4379628107] P3: The header still says the corpus has 371 invocations, but `cases.json` now lists 379 cases after this PR adds six `help__*` cases. Update the count in the intro paragraph to match.
> [4379642453] P3: The sentence says only "those two lines" differ, but each list gains four lines: the "Security tools:" heading, the deepsec and vvah entries, and a blank separator. Name the heading and separator so the corpus note matches the re-captured files.

**Plan / Notes:** cases.json has 379 cases (373 on main + 6 new help cases); the intro said 371. The 'those two lines' sentence now names the heading, both entries and the blank separator (four lines).

Status: **Fixed** (b7c5c5d)

### QEF-13 – Golden capture script skips the new Security tools group

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648346  (author: cubic-dev-ai[bot], `tests/golden/expected/unknown__list.stderr:31`)
- LUS: 5  |  CS: 1  |  Type: bug

**Original Comment:**
> [4379648346] P2: The new **Security tools** group is missing from `SECTIONS` in `scripts/test/golden-capture.py`. Re-capturing the corpus will silently skip `deepsec` and `vvah`, so add `'Security tools'` to that set.

**Plan / Notes:** Added 'Security tools' to SECTIONS. Also added shannon, deepsec and vvah to PASSTHROUGH: the committed cases call `help <tool>`, and without that a re-capture would run `<tool> --help` through the real launcher and produce different cases. Script compiles; a full re-capture was not run (it needs the captured reference build).

Status: **Fixed** (b7c5c5d)

### QEF-14 – A shannon test deletes env vars instead of restoring them

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648445  (author: cubic-dev-ai[bot], `src/harness/shannon.rs:938`)
- LUS: 4  |  CS: 1  |  Type: bug

**Original Comment:**
> [4379648445] P3: This test removes four environment variables instead of restoring their prior values, so it can erase CI or developer settings and make later tests order-dependent. Snapshot and restore each value.

**Plan / Notes:** The test now snapshots the four variables and restores each (set or remove) after the child run.

Status: **Fixed** (b7c5c5d)

### QEF-15 – VVAH config writes the model id as a bare YAML scalar

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648352  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:135`)
- LUS: 4  |  CS: 2  |  Type: bug

**Original Comment:**
> [4379648352] P2: Serialize `model` as a quoted YAML scalar here; accepted IDs may contain `{}`, commas, and `:`, which can break this flow mapping or inject fields.

**Plan / Notes:** Verified `model_id_is_safe` does not forbid `{`, `}`, `,` or `:`. The id is now a double-quoted (JSON) scalar in every role. Test covers an id like `a},b: {c:d`.

Status: **Fixed** (b7c5c5d)

### QEF-16 – Printed VVAH install command leaves paths unquoted

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628079  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:405`)
- LUS: 4  |  CS: 2  |  Type: bug

**Original Comment:**
> [4379628079] P2: The printed install command leaves executable and venv paths unquoted, so paths containing spaces fail when users run it. Quote each path in the displayed command.

**Plan / Notes:** Paths are shell-quoted (single quotes on Unix, double quotes on Windows) only when they hold characters outside a plain-safe set, so ordinary output is unchanged. Test covers a path with a space.

Status: **Fixed** (b7c5c5d)

### QEF-17 – Home-directory guard ignores USERPROFILE (Windows, HOME unset)

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642387  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:660`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642422  (author: cubic-dev-ai[bot], `src/harness/deepsec.rs:503`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648309  (author: cubic-dev-ai[bot], `src/harness/deepsec.rs:503`)
- LUS: 4  |  CS: 1  |  Type: bug

**Original Comments:**
> [4379642387] P1: On Windows shells that leave `HOME` unset, a bare invocation from the user profile bypasses this guard and scans the whole profile. Fall back to `USERPROFILE` on Windows.
> [4379642422] P2: On native Windows with `HOME` unset, this leaves the home-directory guard disabled even though Windows exposes the user's directory through `USERPROFILE`. Resolve the home path using the platform-appropriate variable so a bare launch from the profile is refused.
> [4379648309] P2: On Windows shells that leave `HOME` unset, this passes no home path to the safeguard, so bare `wally deepsec` can run `init` from the user's profile directory. Fall back to nonempty `USERPROFILE` on Windows before calling `refuse_home_workspace`.

**Plan / Notes:** Added `hosted::home_directory()` (HOME, else non-empty USERPROFILE) used by both deepsec and vvah. Pure safety add. The separate point that the guard only fires with no args is flagged in the design section.

Status: **Fixed** (b7c5c5d)

### QEF-18 – Footer example for bare `wally vvah` says it only scans

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628156  (author: cubic-dev-ai[bot], `src/commands/cmd_harness.rs:150`)
- LUS: 4  |  CS: 1  |  Type: docs

**Original Comment:**
> [4379628156] P3: The footer example for bare `wally vvah` says it only scans. With no arguments it also runs `--remediate`, which can edit files in the target repo. Name that in the example text so the footer matches the help and runtime warning.

**Plan / Notes:** Verified classify() turns bare `wally vvah` into `scan --repo . --remediate`. The example now reads 'Scan and remediate (edits files in) the current repo'. This is a help string, so help__vvah and help__help__vvah goldens changed by that one line; the golden test passes.

Status: **Fixed** (b7c5c5d)

### QEF-19 – wally deepsec only refuses three of the seven injected flags

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379567162  (author: coderabbitai[bot], `src/harness/deepsec.rs:72`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628070  (author: cubic-dev-ai[bot], `src/harness/deepsec.rs:72`)
- LUS: 4  |  CS: 2  |  Type: bug

**Original Comments:**
> [4379567162] ** Minor** · **Functional Correctness** · **Quick win** **Reject every flag that the launcher injects.** `route_flags` injects these flags: - `--agent` - `--model` - `--ai-provider` - `--ai-credential-header` `OWNED_FLAGS` refuses only `--model-auth`, `--ai-base-url` and `--ai-api-key-env`. Suppose a user runs `wally deepsec -- process --model glm-5.3-flash`. The user's `--model` is appended after the injected flags. If the later value wins, Pi receives a bare `glm-5.3-flash` without the `runanywhere/` provider pre ...
> [4379628070] P2: Routed commands still accept caller-supplied `--ai-provider` after Wally injects `runanywhere`, allowing DeepSec's per-command provider option to override or conflict with Wally's route. Reject all injected route flags, including `--agent`, `--model`, and `--ai-credential-header`.

**Plan / Notes:** A user's later --agent/--model/--ai-provider/--ai-credential-header can break the route (a bare --model is not in the generated models.json). All seven flags route_flags injects are now refused, with the message pointing at wally's own -m. Test extended.

Status: **Fixed** (b7c5c5d)

### QEF-20 – Fallback note says 'on PATH' for a workspace install

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648363  (author: cubic-dev-ai[bot], `src/harness/deepsec.rs:426`)
- LUS: 3  |  CS: 1  |  Type: nit

**Original Comment:**
> [4379648363] P3: When the preferred workspace install has a mismatched version, this note incorrectly says the binary is on PATH. Use source-neutral wording such as “the installed deepsec” in both fallback notes.

**Plan / Notes:** Both fallback notes now say 'the installed deepsec'.

Status: **Fixed** (b7c5c5d)

### QEF-21 – 'using default model' is printed when the default was unavailable

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379724732  (author: cubic-dev-ai[bot], `src/harness/shannon.rs:719`)
- LUS: 3  |  CS: 1  |  Type: nit

**Original Comment:**
> [4379724732] P3: The "using default model" status line also prints when the default is missing and `choose_model` fell back to the first model. Reword it to name the model without claiming it is the default.

**Plan / Notes:** Verified choose_model falls back to the first model. The status line (shannon and the shared hosted path) now reads 'using <model>'. The same phrase in cmd_default_models.rs is a different, accurate flow and was not touched.

Status: **Fixed** (b7c5c5d)

### QEF-22 – Price request still sent after the model list failed

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379724746  (author: cubic-dev-ai[bot], `src/harness/catalog_models.rs:125`)
- LUS: 3  |  CS: 1  |  Type: nit

**Original Comment:**
> [4379724746] P3: `account_models_with` still fetches prices after the model list has failed. Callers such as `shannon` exit on that failure, so the second request only adds latency. Return early when `result` is not `Ok`.

**Plan / Notes:** `account_models_with` now returns early when the model list is not Ok.

Status: **Fixed** (b7c5c5d)

### QEF-23 – Config containment check fails open when the profile dirs do not exist

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648431  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:213`)
- LUS: 3  |  CS: 2  |  Type: bug

**Original Comment:**
> [4379648431] P3: The containment check fails open when the profile directory and its parent do not exist yet. The config then lands inside the scan root, so VVAH will refuse the run. Walk up to the nearest existing ancestor before comparing.

**Plan / Notes:** `existing_or_parent` now walks up to the nearest existing ancestor. Test covers a not-yet-created path inside and outside the scan root.

Status: **Fixed** (b7c5c5d)

### QEF-24 – An unavailable `-m` model is accepted for deepsec and vvah

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628009  (author: cubic-dev-ai[bot], `src/harness/hosted.rs:170`)
- LUS: 5  |  CS: 2  |  Type: bug

**Original Comment:**
> [4379628009] P1: `catalog_models_with` inserts `primary` when the account list omits it, so `choose_model` accepts unavailable `-m` values and never falls back from an unavailable preference. Select from the unmodified account model list instead.

**Plan / Notes:** Verified: `hosted::load_models` called `catalog_models_with`, which inserts the requested model when the account list omits it, so `choose_model` never refused it. It now uses the account's own list (`account_models_with`, one fetch instead of two) so `-m nonexistent` is refused with the not-available message, as documented. Shannon already used the account list. Treated as a correctness bug (stated intent), not a design change.

Status: **Fixed** (b7c5c5d)

## Section 2 – Larger / Design & Security Decisions (author's call – behavior NOT changed)

No GitHub issues were opened. Each item is documented with a recommendation and flagged to the author on the PR (one consolidated comment plus a pointer on each thread). Behavior is unchanged for all of these.

### DECISION-1 – Home-directory guard fires only when no arguments are given

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642365  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:291`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642443  (author: cubic-dev-ai[bot], `src/harness/deepsec.rs:49`)
- LUS: 4  |  CS: 3

**Original Comments:**
> [4379642365] P1: The home-directory guard only triggers when no arguments are given. `wally vvah --remediate` or `wally vvah scan --remediate` run from $HOME still target `.` (the home directory) and can edit files there. Refuse the home-directory target whenever no explicit `--repo` is given, not only for bare invocations.
> [4379642443] P2: The home-directory guard only applies when no arguments are passed. `wally deepsec init` run from `$HOME` gets past it and can still treat home as the workspace. Base the check on the effective routed command and whether `--repo` is given, not on `args.is_empty()`.

**Finding (verified against the code):** `wally deepsec init` or `wally vvah scan --remediate` from $HOME still target the home directory (deepsec.rs `refuse_home_workspace` and vvah.rs both return false when args is non-empty). For VVAH this can edit files in $HOME.

**Recommendation:** Base the check on the effective target, not on `args.is_empty()`: refuse when the routed command would run in cwd == home and no explicit --repo (VVAH) / no explicit path (DeepSec) was given; keep allowing an explicit --repo to a real project. Add a test per case. Needs the author to confirm what DeepSec treats as its workspace (it uses cwd), so the rule is 'routed command + cwd is home'.

Status: **Flagged to author**

### DECISION-2 – VVAH turns remediation (file edits) on for every routed scan

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642376  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:173`)
- LUS: 5  |  CS: 3

**Original Comment:**
> [4379642376] P1: This enables S10 for every routed scan, so `wally vvah -- scan --repo …` can edit files even without `--remediate`. Enable remediation only when the user explicitly requests it.

**Finding (verified against the code):** `config_yaml` always writes `step_remediate: enabled: true` (vvah.rs, config text), so `wally vvah -- scan --repo X` can edit files without --remediate. Only the bare invocation is documented as remediating.

**Recommendation:** Write `step_remediate.enabled` from the arguments: true only when --remediate (or the `remediate` command) is present, otherwise false. Keep bare `wally vvah` as it is (documented) or make it scan-only and require an explicit --remediate: the author's product call. Add a test on config_yaml(enabled).

Status: **Flagged to author**

### DECISION-3 – DeepSec runs an executable from the scanned repo before any trust check

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642396  (author: cubic-dev-ai[bot], `src/harness/deepsec.rs:312`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379648302  (author: cubic-dev-ai[bot], `src/harness/deepsec.rs:318`)
- LUS: 5  |  CS: 3

**Original Comments:**
> [4379642396] P1: `workspace_deepsec` runs an executable from the scanned repo before any trust check. A repository under test can commit `.deepsec/node_modules/.bin/deepsec`. That file then runs with your full environment for the version probe, and with the account bearer in `WALLY_DEEPSEC_API_KEY` for the real run. Only use a workspace install that wally itself created, or fall back to PATH/npx for untrusted repositories.
> [4379648302] P1: `workspace_deepsec` prefers an executable from the current directory (`.deepsec/node_modules/.bin/deepsec`). That file often comes from the repo being audited, so running `wally deepsec` there executes repo-supplied code, and the probe runs it before any key is set. The child then also receives `WALLY_DEEPSEC_API_KEY`. Only trust a DeepSec install outside the workspace, or require explicit opt-in before running a workspace binary.

**Finding (verified against the code):** `workspace_deepsec(cwd)` prefers `<cwd>/.deepsec/node_modules/.bin/deepsec`; a repository under audit can ship that file. It runs for the version probe with the user's full environment and then, if the version string matches, for the real run with WALLY_DEEPSEC_API_KEY (the account bearer).

**Recommendation:** Do not execute anything from the scanned tree: drop the workspace candidate and use only PATH or the pinned npx, or run the probe with a scrubbed environment and require the file to be owned by the user and outside the scan target. Dropping it is the smallest safe change; it only costs a re-download when the workspace install is the sole copy.

Status: **Flagged to author**

### DECISION-4 – VVAH uses a vvaharness from PATH without checking its version

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642403  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:507`)
- LUS: 4  |  CS: 2

**Original Comment:**
> [4379642403] P2: A `vvaharness` found on PATH is used without checking its version, even though the launcher claims to run the pinned 1.5.1 release. That binary still receives the account bearer as `OPENAI_API_KEY`. Verify the version, or only use the venv-installed binary.

**Finding (verified against the code):** `resolve_vvaharness` falls back to any `vvaharness` on PATH, yet the launcher claims to run the pinned 1.5.1 and hands that binary OPENAI_API_KEY (the account bearer).

**Recommendation:** Probe `--version` and require exactly 1.5.1 (as the shannon and deepsec launchers do), else treat as not installed and offer the venv install; or use only the profile venv binary.

Status: **Flagged to author**

### DECISION-5 – VVAH install pins a mutable git tag, and the pin constant can drift

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379642413  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:27`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628127  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:21`)
- LUS: 4  |  CS: 2

**Original Comments:**
> [4379642413] P2: The install source pins a mutable git tag (`@v1.5.1`) and no commit hash, and the installed tool receives the account bearer. Pin the install to a full commit SHA so a moved or replaced tag cannot change the code that runs with credentials.
> [4379628127] P3: `PINNED_VERSION` is only used by a test that asserts its own value, while `SOURCE` hard-codes `@v1.5.1` separately. Bumping one without the other installs a different release than the one the constant reports. Derive `SOURCE` from `PINNED_VERSION`, or drop the constant and check it against the installed version.

**Finding (verified against the code):** `SOURCE` ends in `@v1.5.1`, a movable tag, and the installed tool receives the bearer. Separately `PINNED_VERSION` is only asserted against its own value while `SOURCE` repeats the number, so bumping one silently installs another release.

**Recommendation:** Pin to the full commit SHA of the v1.5.1 tag (needs someone to resolve and record it), and build SOURCE from a single constant (const_format or a test asserting SOURCE contains PINNED_VERSION and a 40-hex commit). Not done here because the commit hash must be taken from the upstream repository by the author.

Status: **Flagged to author**

### DECISION-6 – VVAH config staged in a shared, predictable temp path follows a pre-existing symlink

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379567184  (author: coderabbitai[bot], `src/harness/vvah.rs:249`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628024  (author: cubic-dev-ai[bot], `src/harness/vvah.rs:283`)
- LUS: 5  |  CS: 3

**Original Comments:**
> [4379567184] ** Major** · **Security & Privacy** · **Quick win** **Do not stage the VVAH config in a shared, predictable temp directory.** The fallback directory is `std::env::temp_dir().join("wally-vvah")`. On Unix this is `/tmp/wally-vvah`, which any local user can create first. Neither `stage_vvah_config` nor `write_private_file` checks who owns an existing directory or what its mode is. `write_private_file` sets mode `0700` only when it creates the parent itself. `std::fs::copy` writes `config.yaml` into that directory and  ...
> [4379628024] P1: The fallback config directory is a predictable path under shared temp, and this copy follows a pre-existing `config.yaml` symlink. Create and verify a private directory, then write the default without following an attacker-controlled destination.

**Finding (verified against the code):** The fallback directory is `temp_dir()/wally-vvah` (`/tmp/wally-vvah`). Any local user can create it first, and `std::fs::copy` of the packaged default.yaml to `config.yaml` follows a pre-planted symlink there. This only occurs when the profile directory is inside the scan target.

**Recommendation:** Use a freshly created private directory (`tempfile::Builder` / mkdtemp, mode 0700) per run and remove it afterwards, or verify the directory is owned by the user, mode 0700 and not a symlink, and write the copy with create_new/no-follow instead of `fs::copy`.

Status: **Flagged to author**

### DECISION-7 – A valid WALLY_CLOUD_KEY is blocked when saved session state cannot be loaded

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628032  (author: cubic-dev-ai[bot], `src/harness/hosted.rs:34`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628087  (author: cubic-dev-ai[bot], `src/harness/shannon.rs:541`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379567176  (author: coderabbitai[bot], `src/harness/hosted.rs:54`)
- LUS: 4  |  CS: 3

**Original Comments:**
> [4379628032] P2: The Cloud-key path still requires `account::load()` to succeed, so corrupt or unreadable saved session state blocks a valid `WALLY_CLOUD_KEY`. Resolve the key before requiring session credentials and obtain the console URL independently.
> [4379628087] P2: A malformed local credentials file makes `account::load()` return before this code checks `WALLY_CLOUD_KEY`, blocking a valid Cloud-key scan. Check the Cloud key first and use the configured console URL when loading the stored session fails.
> [4379567176] ** Minor** · **Stability & Availability** · **Quick win** **Allow `WALLY_CLOUD_KEY` when account loading fails.** `account::load()` already supplies `default_console_url()` when the credentials file is missing, so the empty-URL path does not occur in that case. However, `hosted_access` loads credentials before checking `WALLY_CLOUD_KEY`. When no profile directory is available, the cloud key cannot be used. Read the environment key first. If it is set, use `account::default_console_url()` when credential loading fai ...

**Finding (verified against the code):** Both `hosted::hosted_access` and shannon.rs's copy call `account::load()` before looking at WALLY_CLOUD_KEY, so a corrupt or unreadable credentials file, an insecure profile directory, or no profile directory fails a scan that has a Cloud key (the CI case the key is for).

**Recommendation:** Read the Cloud key first; if set, use `account::load()` when it works and otherwise `normalize_console_url(default_console_url())` (still validated), and still refuse an unsafe key. Left alone because `load()` also fails on purpose for an insecure profile directory and an invalid WALLY_CONSOLE_URL, so which failures may be ignored is an auth-path decision.

Status: **Flagged to author**

### DECISION-8 – Shannon duplicates the shared hosted-account and model-selection helpers

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628173  (author: cubic-dev-ai[bot], `src/harness/shannon.rs:540`)
- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379724775  (author: cubic-dev-ai[bot], `src/harness/hosted.rs:107`)
- LUS: 3  |  CS: 3

**Original Comments:**
> [4379628173] P3: This duplicates the shared hosted-account and model-selection helpers in `src/harness/hosted.rs`, so fixes to session handling can drift between harnesses. Reuse the shared helpers and keep only Shannon-specific model loading here.
> [4379724775] P3: `hosted::not_available_message` duplicates `shannon::not_available_message` exactly. Keep one shared implementation and have Shannon reuse it so account-access guidance cannot diverge.

**Finding (verified against the code):** shannon.rs has its own `hosted_access`, `Hosted`, `choose_model` and `not_available_message` that duplicate `harness/hosted.rs`; fixes (such as the two above) must be made twice.

**Recommendation:** Make shannon call the shared helpers and keep only Shannon-specific model loading. Pure refactor across the launch path of an already working command, so left to the author to sequence with the Cloud-key fix.

Status: **Flagged to author**

### DECISION-9 – Shannon is listed under Coding tools while deepsec and vvah are under Security tools

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628102  (author: cubic-dev-ai[bot], `tests/golden/expected/version__bare.stderr:23`)
- LUS: 3  |  CS: 3

**Original Comment:**
> [4379628102] P3: `shannon` is listed under "Coding tools", but `deepsec` and `vvah` sit in the new "Security tools" group. The `cmd_harness.rs` comment says Shannon is not a coding harness, so move it into "Security tools" in `src/app.rs` and `src/cli_formatter.rs` and re-capture this golden file.

**Finding (verified against the code):** version__bare.stderr lists `shannon` with the coding tools. cmd_harness.rs already calls it 'not a coding harness'.

**Recommendation:** Move shannon into the Security tools group in src/app.rs and src/cli_formatter.rs and re-capture every golden that prints the command list (about 49 files). Cosmetic and touches the command-list layout; the author's call.

Status: **Flagged to author**

### DECISION-10 – A slash-containing model id yields a Pi --model that models.json does not register

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#discussion_r4379628058  (author: cubic-dev-ai[bot], `src/harness/deepsec.rs:142`)
- LUS: 2  |  CS: 2

**Original Comment:**
> [4379628058] P2: A slash-containing model id produces a Pi `--model` value that `models.json` does not register. `pi_model_id` keeps the `zai/` prefix, while `models_json` registers only `runanywhere/`. Build both from the same normalized id so the provider and model match.

**Finding (verified against the code):** `pi_model_id` keeps an id that contains '/' as-is while `models_json` registers only the part after the last '/'. Checked: `-m` ids containing '/' are already rejected by `model_id_is_safe`, so only an account-listed id with a '/' could trigger it.

**Recommendation:** Probably unreachable today. If account ids can contain '/', build both the flag and the registered id from one normalised id (always `runanywhere/<full id>`). Left for the author to confirm whether such ids exist.

Status: **Flagged to author**

## Other comments

### Siddhesh2377 – screenshots of VVAH and DeepSec runs

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#issuecomment-6420901716
- Author: Siddhesh2377 (PR conversation)

**Original Comment:** two screenshots labelled VVAH and DeepSec, addressed to the author. No actionable request in text; the images could not be read in this triage.

Status: **Noted – no action** (flagged to the author for his review of the screenshots)

### CodeRabbit – review-paused status comment

- Source comment: https://github.com/RunanywhereAI/wally/pull/219#issuecomment-6391273196
- Author: coderabbitai[bot] (PR conversation)

**Original Comment:** automated 'Reviews paused' note plus walkthrough (last reviewed commit 5efe277; 730da93 not reviewed). No action item.

Status: **Noted – no action**

## Summary & Status

| | Comments | Distinct entries |
|---|---|---|
| Total triaged (incl. replies) | 66 | |
| Already fixed by the author (b2912b1), with their replies | 15 | 5 |
| Fixed in this pass (`b7c5c5d`) | 32 | 24 |
| Flagged to the author (design / security, behavior unchanged) | 17 | 10 |
| Noted, no action (screenshots, bot status) | 2 | 2 |

Check: 15 + 32 + 17 + 2 = 66 (matches 66).

Not done / caveats:
- Windows ACL enforcement for the config file (docs now state the limitation).
- A full golden re-capture (needs the reference build); the one changed help string was updated by hand and the replay test passes.
- Section 2 items: see recommendations above; none changed.

Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --release --lib -- --test-threads=1` (618 passed), `cargo test --release --test cli_golden` (379 cases), check-agents-sync, check-retired-model-ids, check-versions: all pass.
