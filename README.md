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
  `report`, `ask`, `handback`, `ack`. A run with no terminal report
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

## Build and test

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --offline          # 27 unit tests
cargo build --release --offline
./target/release/dsh-superagent-rs demo
./target/release/dsh-superagent-rs deep-test --out deep_test_results.jsonl [--bank bank.json] [--deepseek-live]
./target/release/dsh-superagent-rs muse-spark-status
./target/release/dsh-superagent-rs run-worker --model muse-spark [--fallback-deepseek]
./target/release/dsh-superagent-rs run-worker --model interactive --bank-out muse_spark_bank.json
./target/release/dsh-superagent-rs run-worker --model bank --bank muse_spark_bank.json
```

See `DEEP_TEST.md` for the extensive deep-test results, including
the Muse Spark interactive leg and the DeepSeek backup leg.
