# Development workflow

Conventions for working on this project. The goal is a clean public history where every
commit on `main` is a verified, working state. This is built incrementally, **one module
at a time**.

## Git & releases

- **One git worktree per module — mandatory.** Never develop directly on `main`.

  ```bash
  git worktree add ../whatsapp-rust-GTK4.worktrees/step-N-short-name -b step-N-short-name
  ```

  Worktrees live in a sibling directory (`../whatsapp-rust-GTK4.worktrees/`), outside the
  main checkout, so the main working tree stays uncluttered.

- **`main` is always clean and runnable.** Only completed, user-verified modules land there.
- **Local commits per verified module.** Pushing/tagging to GitHub happens **only at a
  release** — not per module.
- After verification: merge the worktree branch into local `main`, then remove the worktree
  and delete the merged branch:

  ```bash
  git worktree remove ../whatsapp-rust-GTK4.worktrees/step-N-short-name
  git branch -d step-N-short-name
  ```

- **Conventional Commits** (`feat:`, `fix:`, `docs:`, `chore:`, …). One commit = one
  verified, working state.
- **`Cargo.lock` is committed** (this is an application). `.gitignore` ignores only `/target`.

## Quality gates (before a module is "done")

- **End-to-end verification:** clean build **and** runtime behavior confirmed by actually
  running the app.
- `cargo fmt` and `cargo clippy` produce **no warnings** before merging.
- **No `unwrap`/`expect`/`panic`** on runtime/IO/network paths in non-test code; use
  explicit error handling. Decryption and connection errors are **logged, never fatal**.
- Crate versions are **pinned exactly** (`=x.y.z`); upgrades are deliberate (the protocol
  backend is young and changes often).
- New **system dependencies** are documented in the README as soon as they are introduced.

## Process

- One module at a time; the next module starts only after the current one is verified.
- Keep the README and this document up to date as conventions evolve.
