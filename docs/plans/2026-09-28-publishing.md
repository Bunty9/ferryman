---
title: ferryman — publishing to crates.io
status: 0.2.0 published 2026-09-29; release.yml workflow added
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

## Release log

- 2026-09-28: `ferryman-core` 0.1.0 and `ferryman` 0.1.0 published with
  `cargo publish --workspace`; tag `v0.1.0` and GitHub release created;
  `cargo install --locked ferryman` verified from the registry.

## Step 2 — pre-flight (every release)

```bash
export PATH=$HOME/.cargo/bin:$PATH
git switch main && git pull && git status          # clean tree
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
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

`.github/workflows/release.yml` exists in the repo already. On a tag push
(`v[0-9]+.[0-9]+.[0-9]+` or prerelease `v[0-9]+.[0-9]+.[0-9]+-*`) it
runs five jobs, each with only the permissions and checkout it needs
(see "why separate jobs" below). Graph: `verify` -> `binaries` ->
`attest` -> `publish` -> `release`, with `binaries-extra` (best effort) alongside:

1. **`verify`** (`permissions: contents: read`, checkout with
   `persist-credentials: false`) — checks the tag points at a commit on
   `main` (`git merge-base --is-ancestor`), checks the tag matches
   `[workspace.package]` version and that the `ferryman-core` entry in
   `[workspace.dependencies]` agrees, extracts the matching
   `CHANGELOG.md` section into `release-notes.md` and uploads it as a
   build artifact, then runs `cargo test --workspace --locked`.
2. **`attest`** (needs `verify` and `binaries`; `id-token: write`,
   `attestations: write`, `contents: read`; no checkout, no cargo) —
   downloads the tier-1 `bin-t1-*` archives and runs
   `actions/attest-build-provenance` on them. Tier-2 archives are not
   attested (so best-effort builds never delay publish). Skipped on dry
   runs.
3. **`publish`** (needs `verify`, `binaries` and `attest`, so nothing irreversible
   happens unless every tier-1 archive built; `permissions: id-token: write,
   contents: read`, checkout with `persist-credentials: false`) —
   authenticates via `rust-lang/crates-io-auth-action@v1` (Trusted
   Publishing, no long-lived token), then publishes `ferryman-core` and
   `ferryman`, **in that order, one crate at a time**: for each, it first
   checks whether that exact `name/version` already exists on crates.io
   and skips it if so, otherwise runs `cargo publish -p <name> --locked`.
   This makes the job idempotent — see "recovering from a half-published
   release" below.
4. **`release`** (needs `verify`, `publish`, `binaries`, `binaries-extra`,
   `attest`; runs under `!cancelled()` when verify, publish, `binaries`
   and `attest` all succeeded, ignoring `binaries-extra`; `permissions: contents: write`,
   no checkout) — downloads `release-notes` and every `bin-*` artifact,
   fails if a tier-1 archive is missing, writes `SHA256SUMS`, and runs
   `gh release create` with all archives attached (or `gh release upload
   --clobber` if the release exists). No `cargo`, no dependency code, by
   design.

5. **`binaries`** (tier 1) and **`binaries-extra`** (tier 2, `continue-on-error`)
   (need `verify`; `contents: read`, no credentials, no OIDC) — matrices that builds `ferryman` with
   `--locked --release` and packages
   `ferryman-v<version>-<target>.tar.gz` (`.zip` on Windows; top-level
   dir with binary, README, CHANGELOG, licenses, `config.toml`) plus a
   `.sha256`. Tier 1 (Linux musl x86_64/aarch64, macOS x86_64/aarch64,
   Windows x86_64 MSVC) gates `attest`, `publish` and `release`; tier 2 (ARM/i686
   musl and riscv64 gnu via `cross`, FreeBSD, Windows aarch64) may be
   absent. `--prerelease` is set for `-` tags. Prerelease tags (`vX.Y.Z-rc.1`) must equal the workspace
   version like any other.

**Dry run.** Actions tab, *release*, *Run workflow* (`workflow_dispatch`)
runs only `binaries` and `binaries-extra` (verify/attest/publish/release are skipped);
download the `bin-t1-*`/`bin-t2-*` artifacts to inspect the archives. Do this before
the first tagged release that ships binaries, and after changing the
matrix.

`cargo binstall` finds the archives through
`[package.metadata.binstall]` in `crates/server/Cargo.toml`; keep that
URL template in step with the archive naming in `release.yml`.

**Why separate jobs.** A single job would run `cargo test` and
`cargo publish` — both of which execute arbitrary third-party code
(`build.rs`, proc macros, test binaries) — in the same process context as
the `id-token: write` OIDC token (used to mint the crates.io publish
token) and a `contents: write` `GITHUB_TOKEN` persisted by `actions/
checkout` into `.git/config`. Any dependency's build script could read
`ACTIONS_ID_TOKEN_REQUEST_TOKEN`/`_URL` or `git config` for the repo
token. Splitting into jobs means the job that runs untrusted code
(`verify`, `publish`) never holds `contents: write`, and the job that
holds `contents: write` (`release`) runs no untrusted code and doesn't
even check out the repository.

**One manual step is left**, and it's done in the crates.io web UI (no
API/CLI for it): for **both** crates — `ferryman` and `ferryman-core` —
go to the crate's page on crates.io, *Settings* → *Trusted Publishing* →
*Add GitHub*:

- Repository owner: `Bunty9`
- Repository name: `ferryman`
- Workflow filename: `release.yml`
- Environment: (leave blank — the job doesn't use one)

Until that's done for both crates, `cargo publish` in the workflow will
fail with an OIDC/auth error; the long-lived token from Step 3 works as a
fallback in the meantime but isn't used by the workflow.

### Recovering from a half-published release

`ferryman` depends on `ferryman-core`, so the `publish` job always does
`ferryman-core` first. If `ferryman-core` publishes successfully but
`ferryman` then fails (network blip, crates.io hiccup, a transient CI
issue), crates.io now has the new `ferryman-core` but not `ferryman`, and
no GitHub release was created (the `release` job needs `publish` to
succeed first). Because `publish` needs the tier-1 `binaries` and `attest` jobs, a
deterministic build or attestation failure now stops the release
before anything reaches crates.io (re-run the failed job and the rest
follows), so it cannot leave a half-published
release; only transient failures after publish starts can.

Because each crate's publish step checks crates.io for that exact
`name/version` before publishing, **re-running the failed jobs** from the
Actions UI (don't re-push the tag) picks up where it left off: `ferryman-core`
is found to already exist and is skipped, `ferryman` gets published, and
the `release` job then creates the GitHub release (it still requires
every tier-1 archive; re-run `binaries` jobs too if they expired). No manual crates.io
intervention needed for this case.

If the workflow can't be re-run (e.g. the tag itself was wrong), publish
`ferryman` by hand — `cargo publish -p ferryman --locked` from a clean
checkout of the tagged commit, with a crates.io token via `cargo login`
— and then create the release manually: `gh release create vX.Y.Z
--notes-file <(...)` using the same `CHANGELOG.md` section (see the
`awk` command in `release.yml`'s "Extract CHANGELOG.md section" step).

### Release procedure (once Trusted Publishing is set up)

1. Bump the version in **`[workspace.package]`**, the `ferryman-core`
   entry of **`[workspace.dependencies]`** in the root `Cargo.toml`, and
   — on a minor/major bump only — the `version = "0.2"`-style
   requirement on both `ferryman` and `ferryman-core` in
   `examples/embedded/Cargo.toml` (a path dependency's `version` req
   still has to be satisfied by the local package's version even though
   the path is what's actually built against; a stale `"0.2"` after a
   bump to `0.3.0` fails workspace resolution).
2. Add a dated `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md` (move
   `[Unreleased]` content into it).
3. Run `cargo test --workspace --locked` (or any `cargo` command) once
   locally so `Cargo.lock` picks up the version bump, and commit the
   refreshed `Cargo.lock` along with the version/changelog changes —
   `release.yml` builds with `--locked` throughout, so a stale lockfile
   fails the release instead of silently resolving something else.
4. Commit that to `main` and wait for CI to go green on the commit.
5. Tag it and push the tag:
   ```bash
   git tag -a vX.Y.Z -m "ferryman X.Y.Z"
   git push origin vX.Y.Z
   ```
6. `release.yml` runs on the tag push: tests, publishes both crates to
   crates.io, and creates the `vX.Y.Z` GitHub release from the
   `CHANGELOG.md` section. Nothing further to do by hand — and if any
   step fails partway, see "recovering from a half-published release"
   above.

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
