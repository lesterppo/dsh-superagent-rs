# dsh-superagent-rs

Repo: https://github.com/lesterppo/dsh-superagent-rs

Rust port of the **core** of
[deepseek-ai/dsh-superagent-prototype](https://github.com/deepseek-ai/dsh-superagent-prototype)
(MIT, research prototype for reference only, not an official DeepSeek
product). Original cloned 2026-11-02 into
`~/workspace/dsh-superagent-prototype/` (read-only reference; this crate
does not modify it).

## What is ported, and what is not

Ported, with the original's semantics and invariants:

- Domain model (`src/model.rs`, from `src/core/model.ts`):
  Trigger / Thread / Eval / Mandate / Ticket / Assignment /
  Subscription, `followupOwners`, `settled`, `reached`, `followsDone`,
  `waitsForPerson`, `readUntil`, `isoTime`, `threadKey`.
- Policy gate (`src/policy.rs`, from `src/policy.ts` +
  `src/admission.ts`): listed-addressed is always explicit; admission
  defaults listedUnaddressed=inferred, unlistedAddressed=ignore,
  unlistedUnaddressed=ignore; follow-up, bot, and explicit-cell rules;
  `policyVersion` (mandate-v3, SHA-256 truncated to 32 hex);
  `assessmentOutcome` (settled direction + matched enabled rule +
  intent evidence; inferred accept outside `auto` becomes
  would-accept ignore).
- Engine (`src/engine.rs`, from `src/core/engine.ts`,
  `intake.ts`, `evals.ts`, `tickets.ts`, `assignments.ts`):
  idempotent intake, subscription fan-out (context vs own),
  one Eval per thread, pending batching, follow-ups staying with
  their ticket's worker, expiry requeue up to `REQUEUES = 2` then
  `unavailable`, pause shifting deadlines, accept creating exactly
  one ticket, `bindThread`, `threadClosed` + `followsDone`,
  worker reports (progress/waiting/ready/blocked/failed/dropped),
  attention questions, handback (once per ticket, re-queued for
  fresh Eval when no other ticket decides it), ack, retry, requeue.
- Agent loop (`src/agent.rs`, from `prompts/orchestrator.md`,
  `prompts/worker.md`, `prompts/work.md` and the tool shapes in
  `src/app/tools/main.ts`): worker tools `exec` (cwd-scoped, 30 s,
  blocked-command list), `read_file`, `write_file`, `list_dir`,
  `report`, `ask`, `handback`, `ack`, plus Muse-agent patterns
  absorbed 2026-11-03 from `~/workspace/hermes-android/
  MUSE_AGENT_PATTERNS.md`: `memory_save` / `memory_search`,
  `search_files`, a tool-call guardrail (identical work-tool calls
  capped at 2 in a row, per-tool cap 20, A-B-A-B cycle detection,
  failure budget 8), a 16k-char conversation window that drops
  whole assistant+tool blocks only, structured tool outcomes
  (`OK:` / `ERROR:`), a finish gate that rejects empty,
  placeholder-PR, or no-work `ready` reports back to the model,
  interrupt via a `STOP` file in the workdir, and a 3-tier prompt
  (`run_worker_with_context`). A run with no terminal report
  is `failed`/`max_turns`, never a silent success. Path escapes and
  absolute paths are refused.
- Outbox: replies the engine would send are recorded, not sent to
  Feishu/GitHub (no live channel adapters in this port).

Deliberately NOT ported (deployment/host-specific, per docs/001):

- The React web panel (`src/client/`, `src/app/views/`).
- DSH session APIs, the DSH executor, and the cordis bundle patch.
- Live Feishu / GitHub source adapters and channel senders.
  The executor here is a local workdir subprocess host, which is
  the faithful local analogue of "one directory per ticket".

## Models: Muse Spark primary (server-hosted), DeepSeek backup

**Live Muse Spark (2026-11-02 wiring, not yet authenticated):**
`run-worker --model muse-spark` uses `MuseSparkProvider`, an
OpenAI-compatible Chat Completions client against Meta's Model API
at `https://api.meta.ai/v1/chat/completions`, model `muse-spark-1.3`
(`muse-spark-1.3-contributor` via `MUSE_SPARK_MODEL`). The model
runs on Meta's servers — nothing is extracted or run locally; this
VM is only the agent loop + tools. Credential, in order:
`MODEL_API_KEY`, then `META_API_KEY`, then `MUSE_SPARK_API_KEY`,
then an authd surrogate for connector `custom.meta`. The key is
never printed. Check config without calling Meta:

```bash
./target/debug/dsh-superagent-rs muse-spark-status [--live]
```

Status on 2026-11-02: `configured: false`, `key_source: none` —
the Meta Model API application is still pending on Peter's own
device, and `custom.meta` does not exist yet. Without a key the
worker exits 2 with NOT_CONFIGURED instead of faking a call.
Verified with a dummy (non-real) key the same day: the client
reaches `api.meta.ai/v1` and Meta replies HTTP 401
`invalid_api_key`, and the worker reports `final_state: error` —
so the wiring is proven end-to-end except for a real key. When the
key lands (Peter completes the application on his own device, then
hands it over via Secure Vault as `custom.meta`; do NOT paste it in
chat, and do NOT help circumvent the Meta geo restriction), the
same command runs live, no code change needed. Add
`--fallback-deepseek` to fall back to DeepSeek on a Muse Spark
failure. `META_API_BASE` / `MUSE_SPARK_API_BASE` override the base
URL.

**In-session Muse Spark (self-test, no key needed):** exactly as
in `~/workspace/model-compare/muse_spark.py`:

- `run-worker --model interactive --bank-out bank.json` — the live
  agent (Muse Spark itself) answers each turn on stdin, terminated
  by a line `<<<END>>>`; every turn is recorded under its bank key.
- `run-worker --model bank --bank bank.json` — deterministic replay
  of those authored answers. Bank keys are SHA-1 of the canonical
  (messages, tools) JSON, truncated to 16 hex, the same scheme as
  `muse_spark.py`. Bank runs are labelled self-test, never a live
  API claim.

**DeepSeek backup:** OpenAI-compatible client for
`https://api.deepseek.com/chat/completions`, model `deepseek-flash`
(DeepSeek V4.1 Flash; `deepseek-chat`/`deepseek-reasoner` are
retired aliases). Credential is an `hsurr:` surrogate from authd,
swapped for the real key only on egress to `api.deepseek.com`.
Note: the egress block on `api.deepseek.com` recorded in the
`custom.deepseek` connector doc NO LONGER HELD as of 2026-11-02
(live DeepSeek calls succeeded that day) — re-check the doc before
relying on it. The client is unit-tested on canned
OpenAI-compatible responses, and `run-worker --model deepseek`
exercises the real path.

## Real-life use

This is the layer that makes the core usable against a real
repo (added 2026-11-03; `src/real.rs`). It does not change the
core semantics — it adds config, persistence, a status view,
git-worktree work, and GitHub polling via the `gh` CLI.

```bash
./target/release/dsh-superagent-rs init --dir my-agent
# Edit my-agent/superagent.json: repo ("owner/repo"), allowed
# GitHub logins, model (deepseek | muse-spark | bank | scripted),
# work_root, state_file, poll interval, blocked commands.
cd my-agent
./target/release/dsh-superagent-rs status --state superagent-state.json

# One ticket, in a worktree branch, reviewed before anything leaves:
./target/release/dsh-superagent-rs work --config superagent.json \
  --repo-path /path/to/repo --branch agent/typo \
  --title "Fix typo" --prompt "Fix the typo in README.md" \
  --model deepseek
# --commit commits the worktree branch (only on a ready run with
# a non-empty diff). --create-pr (needs --commit) opens a PR.
# Nothing pushes, and nothing ever merges, on its own.

# Poll GitHub issues (read-only by default):
./target/release/dsh-superagent-rs poll --config superagent.json --once
# --run-workers needs --repo-path and runs workers for accepted
# tickets in worktrees. --post-replies posts only final/attention
# outbox replies as issue comments. Add --dry-run to print the
# gh commands instead of running them.
# watch mode: omit --once; it polls every poll_interval_seconds.
```

How polling decides: an open issue becomes a trigger only if
it @-mentions the agent account (resolved via `gh api user`)
or carries the `superagent` label; anything else is skipped,
not recorded. The existing policy gate then applies: only
explicit requests from allowed users are accepted (the rule
assessor accepts Explicit and ignores Inferred); unlisted
senders are ignored. Triggers, decisions, and tickets persist
in `superagent-state.json`, so a restart resumes instead of
re-processing (re-polling the same issue creates nothing new).

Safety defaults, stated plainly because they are the point:

- Poll is dry-run unless `--run-workers` is passed.
- Work never pushes; PR creation needs `--create-pr` on that
  exact command, on top of `--commit`; there is no merge flag
  anywhere in this codebase, and no auto-merge path.
- A worktree branch that already exists is refused, so work
  is never silently overwritten; all paths are canonicalized
  to absolute paths before git or the worker touches them
  (a relative-path mismatch that edited a non-git directory
  was caught by the e2e test and is the reason for this).
- The worker still runs only inside its worktree, with the
  blocked-command list, the guardrail, the finish gate, and
  the credential-arg refusal from the earlier batches.
- `verified` on a run is advisory: it means the worker read
  back or ran a check after its last write. A `ready` run
  without it is still `ready` — check the diff yourself
  before committing or opening a PR.

What is still NOT real-life ready, honestly:

- No Feishu adapter, no React panel (`status` is the CLI
  replacement), no DSH sessions (the executor is a local
  worktree subprocess host).
- No auto-reply posting by default: outbox replies are
  recorded in state and only posted with `--post-replies`.
- The model legs cost real API calls (DeepSeek) or need a
  real key (Muse Spark, still pending on Peter's device);
  `scripted` / `bank` runs are for tests, not production.
- Polling uses the `gh` CLI and its saved login; it does
  not handle GitHub webhooks, and it polls on an interval
  rather than reacting instantly.

Proven 2026-11-03 on a throwaway local repo (not a real one):
`init` → `work --model scripted --commit` produced ticket-3,
ready, diff `README.md | 2 +-`, commit `e74d2dc` on branch
`agent/typo`, original repo file untouched, `status` showing
the completed ticket, invariants clean; a second run on the
same branch was refused. A read-only `poll --once` against
`lesterppo/dsh-superagent-rs` resolved own login `lesterppo`,
found 0 matching issues, saved state, and posted nothing.

## Build and test

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --offline          # 46 unit tests
cargo build --release --offline
./target/release/dsh-superagent-rs demo
./target/release/dsh-superagent-rs deep-test --out deep_test_results.jsonl [--bank bank.json] [--deepseek-live]
./target/release/dsh-superagent-rs deepseek-extensive --out deepseek_extensive_results.jsonl
./target/release/dsh-superagent-rs muse-spark-status
./target/release/dsh-superagent-rs run-worker --model muse-spark [--fallback-deepseek]
./target/release/dsh-superagent-rs run-worker --model interactive --bank-out muse_spark_bank.json
./target/release/dsh-superagent-rs run-worker --model bank --bank muse_spark_bank.json
```

See `DEEP_TEST.md` for the extensive deep-test results, including
the Muse Spark interactive leg and the DeepSeek backup leg.
