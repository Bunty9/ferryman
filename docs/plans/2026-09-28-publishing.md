---
title: ferryman — publishing to crates.io
status: 0.1.0 published 2026-09-28
date: 2026-09-28
related:
    - ./2026-09-26-ferryman-phase-2.md
    - ../../CHANGELOG.md
---

# Publishing ferryman to crates.io

> Publishing is permanent: a version can be yanked but never deleted or
> re-uploaded. Everything below up to step 3 is reversible; step 3 is not.

## Current state (done)

- Workspace metadata in `[workspace.package]`: `version`, `edition`,
  `rust-version = "1.88"` (verified with a 1.88 toolchain), `license =
  "MIT OR Apache-2.0"`, `authors`, `repository`, `homepage`.
- Per-crate `description`, `documentation` (docs.rs), `readme`,
  `keywords`, `categories`.
- `LICENSE-MIT` / `LICENSE-APACHE` symlinked into each crate so both texts
  ship in every `.crate` file (cargo follows the symlinks).
- `ferryman-core` has its own `README.md`; its example is also a doctest
  in `lib.rs`, so it can't drift.
- `ferryman` depends on core via `[workspace.dependencies]`
  (`path` + `version`), which cargo rewrites to a registry dependency on
  publish.
- `ConfigToml` / `RouteToml` are `#[non_exhaustive]`, so new config keys
  are not a semver break.
- `cargo publish --workspace --dry-run` packages and verifies both crates;
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` is clean.
- Names `ferryman` and `ferryman-core` were free on crates.io as of
  2026-09-28.

## Step 1 — binary crate name (decided)

The proxy is published as **`ferryman`** (package, library and binary), so
`cargo install ferryman` installs a `ferryman` binary. The library crate
stays `ferryman-core`. The source directory is still `crates/server`.

## Step 2 — pre-flight (every release)

```bash
export PATH=$HOME/.cargo/bin:$PATH
git switch main && git pull && git status          # clean tree
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo publish --workspace --dry-run
```

Also: CI green on the release commit, `CHANGELOG.md` dated, version bumped
in `[workspace.package]` and in the `ferryman-core` entry of
`[workspace.dependencies]` (the two must match).

## Step 3 — first release (manual, irreversible)

1. Log in once: create a token at <https://crates.io/settings/tokens>
   (scope `publish-new` + `publish-update`), then `cargo login`.
2. Publish in dependency order (cargo >= 1.90 orders them for you):
   ```bash
   cargo publish --workspace
   # or: cargo publish -p ferryman-core && cargo publish -p ferryman
   ```
3. Tag and release:
   ```bash
   git tag -a v0.1.0 -m "ferryman 0.1.0" && git push origin v0.1.0
   gh release create v0.1.0 --notes-from-tag
   ```
4. Check <https://docs.rs/ferryman-core> and
   <https://docs.rs/ferryman> built, then add crates.io and docs.rs
   badges to `README.md` (they 404 until the crates exist).

## Step 4 — automate later releases

After the crates exist, enable crates.io Trusted Publishing (crate
settings, then *Trusted Publishing*: repo `Bunty9/ferryman`, workflow
`release.yml`) and drop the long-lived token. Then add:

```yaml
# .github/workflows/release.yml
name: release
on:
  push:
    tags: ["v*"]
jobs:
  publish:
    runs-on: ubuntu-latest
    permissions:
      id-token: write
      contents: write
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@stable
      - uses: rust-lang/crates-io-auth-action@v1
        id: auth
      - run: cargo publish --workspace
        env:
          CARGO_REGISTRY_TOKEN: ${{ steps.auth.outputs.token }}
      - run: gh release create "$GITHUB_REF_NAME" --notes-from-tag
        env:
          GH_TOKEN: ${{ github.token }}
```

It is left out of the repo until Trusted Publishing is configured, since
it can't work before the first manual publish.

## Versioning policy

- Both crates move in lockstep on one workspace version.
- While `0.x`: a minor bump (`0.1` to `0.2`) for any breaking change to
  `ferryman-core`'s public API or to config/CLI behaviour; a patch bump for
  fixes and additions.
- Public API worth freezing deliberately before `1.0`: the `pub` fields on
  `Upstream`, `Route` and `RouteTable::upstream_timeout`, and the
  `Admission` / `CircuitState` enums.
- MSRV changes are a minor bump. Recommend `cargo install --locked`,
  since without it cargo may pick dependency versions newer than the
  MSRV.
