# Activity inputs and limits

SystemScape reads local JSONL files. It never executes commands found in a record,
opens a repository, calls an API or sends history to a model.

| Input | Default location |
|-------|------------------|
| Claude Code | `$HOME/.claude/projects/*/*.jsonl` |
| Claude subagents | `$HOME/.claude/projects/*/*/subagents/*.jsonl` |
| Codex | `$HOME/.codex/sessions/**/*.jsonl` |
| Agentbox profiles | `$WORKSPACE/profiles/*/.claude/projects/` and `*/.codex/sessions/` |
| Shared agent events | `$WORKSPACE/.agentbox/agent-events/events.jsonl` and numbered rotations |

`WORKSPACE` defaults to `/home/devuser/workspace`. `CLAUDE_CONFIG_DIR` and
`CODEX_HOME` add the current harness's history roots. `AGENTBOX_EVENT_ARCHIVE_DIR`
overrides the archive location. `--home PATH`, `--workspace PATH` and
`--archive PATH` provide explicit alternatives. An explicit `--home` also disables
the harness environment roots, which makes fixture runs independent of the caller.

Claude user text is distinguished from tool results and metadata. Prompt IDs and
tool-call IDs prevent common replay duplication. Codex `event_msg.user_message`
records supply prompts; response items supply tool calls; session metadata supplies
the session ID and working directory. Replayed Codex records deduplicate when their
record content or tool-call identity matches. Forks with different session IDs can
still overlap. There is no claim of exact billing or task completion accounting.

Archive actors retain numeric `source_agent_id` values as `archive:<id>`;
`handoff_id` groups events when supplied. Unknown actors remain labelled unknown.
Step IDs deduplicate retransmitted trajectory events. Archives are a complementary
source rather than a join against transcripts, so overlapping records stay visible.

Reads use retained offsets and small checkpoints. Incomplete lines wait for a
later poll; inode replacement, truncation and checkpoint changes reset the reader.
Arbitrary rewrites elsewhere in an already-read file are outside the append-oriented
contract. Long lines and cold histories exceeding the tail limit are skipped and
reported as limited. Discovery is bounded to 32,768 entries, 256 profiles and 256
selected files, with bounded traversal depth and no recursion through child symlinks.

The renderer strips terminal controls and directional formatting from excerpts.
It does not redact credentials typed into prompts: this is a local operator view,
and screenshots or JSON exports can contain sensitive source text. Use `--demo`
when sharing the display.

## Verification

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

Regression tests cover partial appends, replay, rotations and rewrites, numeric
archive identities, pending-file disclosure, invalid shapes, control filtering,
subagent discovery, metadata preservation across a bounded tail, scene limits and
small terminals. [Screenshot capture](SCREENSHOTS.md) exercises both live terminal modes.
