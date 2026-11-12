# fermata

Terminal dashboard (Rust, ratatui) for one-orchestrator, many-workers Claude Code work.

## Rules

- **Design doc first.** `DESIGN.md` is the source of truth. Change it before (or with) any behaviour
  change, and keep it accurate as the code evolves.
- **No outward actions.** fermata never pushes, merges, comments, or transitions tickets. It only
  reads local CLIs/files, and relays typed input to tmux panes plus a tracker `note`.
- Tracker access goes only through baton's `tracker.sh` seam. Read verbs in normal operation;
  `note` is the only write, and only when the user sends input.
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
