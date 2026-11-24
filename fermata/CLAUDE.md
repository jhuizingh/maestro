# fermata

Terminal dashboard (Rust, ratatui) for one-orchestrator, many-workers Claude Code work.

## Rules

- **Design doc first.** `DESIGN.md` is the source of truth. Change it before (or with) any behaviour
  change, and keep it accurate as the code evolves.
- **No outward actions.** fermata never pushes, merges, comments, or transitions tickets. It only
  reads local CLIs/files, relays typed input to tmux panes, and records in the tracker.
- Tracker access goes only through baton's `tracker.sh` seam. The writes are
  `note` and `reply`, only when the user sends input, and `decide` and `resolve`, only for questions
  a session asks in its own pane (`prompts.rs`).
- Ticket systems live in per-context adapters (`<workspace>/fermata.yaml`), never in the core.
- Tests must never touch live tmux sessions or the real tracker: use the fakes in `routing.rs`.
- Lives in the public maestro repo: no employer names, ticket keys, real URLs or personal paths
  in code, tests or docs.

## Build and test

```bash
cargo build --release          # binary: target/release/fermata
cargo test                     # unit tests (no live side effects)
cargo test -- --ignored        # scratch-tmux integration test (creates and kills its own session)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
target/release/fermata --dump  # headless JSON of the joined task list
```

Inline comments: two lines max. Follow `.editorconfig`.
