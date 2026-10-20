# Release process

mftfind is released from **annotated Git tags**. Pushing a tag that matches the
version rules below triggers `.github/workflows/release.yml`, which builds the
binary on a Windows runner and publishes a GitHub Release with the packaged
assets attached.

## Version rules

Semantic Versioning 2.0.0, with a mandatory `v` prefix on the tag.

### Tag format

```
v<major>.<minor>.<patch>
v<major>.<minor>.<patch>-<channel>.<ordinal>
```

Where `<channel>` is one of `alpha`, `beta`, `rc`.

CI accepts a tag only if it matches:

```
^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|beta|rc)\.(0|[1-9][0-9]*))?$
```

| Accepted | Rejected | Why |
| --- | --- | --- |
| `v0.1.0` | `0.1.0` | missing `v` prefix |
| `v0.2.0-alpha.1` | `v0.2.0-alpha` | missing ordinal |
| `v0.2.0-rc.2` | `v0.2.0-rc.01` | leading zero in ordinal |
| `v1.0.0` | `v0.2` | patch component required |
| | `release-0.2.0` | wrong prefix |
| | `v0.2.0-beta` | missing ordinal |
| | `v0.2.0-preview.1` | channel not in the allowed set |

### Channels

| Channel | Tag | GitHub Release | Meaning |
| --- | --- | --- | --- |
| alpha | `vX.Y.Z-alpha.N` | Pre-release | Feature set still moving; expect breakage. Personal use and invited testers. |
| beta | `vX.Y.Z-beta.N` | Pre-release | Feature set frozen. Public testing, bug fixes only. |
| rc | `vX.Y.Z-rc.N` | Pre-release | Release candidate. Only blocking defects may be fixed. |
| stable | `vX.Y.Z` | Latest release | Final. Index format and CLI are as documented. |

SemVer's own ordering does the sequencing for free, so no custom comparison
logic is needed anywhere:

```
0.2.0-alpha.1 < 0.2.0-alpha.2 < 0.2.0-beta.1 < 0.2.0-rc.1 < 0.2.0
```

The ordinal resets to `1` every time the channel advances. **Never reuse a tag
and never re-point an existing tag** — if a build is broken, cut the next
ordinal.

Once a `beta` is published, the target version is frozen: only fixes land until
the final tag.

### Which number to bump

The project is pre-1.0, so the usual SemVer rules are inverted for the top two
components:

| Bump | When |
| --- | --- |
| **PATCH** | Backwards-compatible fixes and internal improvements only. |
| **MINOR** | New features, or anything that breaks compatibility. |
| **MAJOR** | Reserved. Will be used once the index format and CLI surface are frozen. |

Treat these as **breaking** and bump MINOR (or MAJOR once ≥ 1.0):

- **Index file layout changes.** `src/format.rs`:
  - Bumping `VERSION` invalidates every existing `.fst` file. The release notes
    must say **"re-index required"**, otherwise users will hit load failures
    with no explanation.
  - Changing `MAGIC` makes the tool refuse to load old indexes at all. This is
    the last resort, not a normal bump.
- **CLI surface changes**: renaming or removing a subcommand or flag.
- **Raised requirements**: minimum Windows version, toolchain, or CPU feature.

These are **not** breaking, PATCH is fine:

- GUI layout, sorting, and interaction changes
- New optional flags or subcommands
- Performance work that does not change the on-disk format
- Bug fixes

### Version consistency (enforced, not advisory)

The tag version must equal `package.version` in `Cargo.toml` **exactly**. The
`verify` job fails the whole run otherwise — a mismatch would publish a binary
that reports the wrong version, or a zip named after a version that is not in
the source it was built from.

`Cargo.lock` must also be up to date, because CI builds with `--locked`.
`scripts/release.sh` handles both.

## Cutting a release

Prerequisites: clean worktree, on `main`, up to date with `origin`.

```sh
./scripts/release.sh 0.2.0-alpha.1          # bump, refresh lock, commit, tag
./scripts/release.sh 0.2.0-alpha.1 --push   # ...and push, triggering the release
```

Manual equivalent:

```sh
# 1. set package.version in Cargo.toml
# 2. refresh the lockfile (CI builds with --locked)
cargo metadata --format-version 1 > /dev/null
# 3. commit the bump
git add Cargo.toml Cargo.lock
git commit -m "chore(release): v0.2.0-alpha.1"
# 4. annotated tag, matching the version exactly
git tag -a v0.2.0-alpha.1 -m "mftfind v0.2.0-alpha.1"
# 5. push the commit and the tag — this is what starts the release
git push origin main && git push origin v0.2.0-alpha.1
```

## What CI does

### `.github/workflows/ci.yml`

Runs on every push to `main` and every pull request. Builds
`cargo build --locked --release` on `windows-latest` with the MSVC toolchain,
runs `tools/embed_icon.py` on the result, and reports the binary size. Publishes
no artifacts — it answers "does main still compile" and "does the icon still
embed", so a broken packaging step fails on `main` instead of on a tag.

### `.github/workflows/release.yml`

Runs on any `v*` tag push (and can be started manually against an existing tag).

1. **verify** — validates the tag against the rules above, derives the channel
   and pre-release flag, and fails if the tag disagrees with `Cargo.toml`.
2. **build** — `cargo build --locked --release` on `windows-latest`, then
   packages the release zip and a `SHA256SUMS.txt`.
3. **release** — creates the GitHub Release with auto-generated notes (commit
   list since the previous tag), marks pre-releases, and attaches the assets.

Release assets:

```
mftfind-v<version>-x86_64-pc-windows-msvc.zip
  ├── mftfind.exe
  ├── mftfind.bat
  └── README.md
SHA256SUMS.txt
```

Re-running the workflow for an existing tag is safe: the release job uploads
with `--clobber` instead of failing. The tag itself is never moved by CI.

Note that the runner uses the **MSVC** toolchain, while `build.sh` on a
development machine targets **GNU**. Both link the same `kernel32` / `shell32`
imports, so the resulting binaries behave identically; only the toolchain
metadata differs, which is why it appears in the artifact name.

### Recovering from a bad tag

If a tag was pushed with the wrong version in the source:

```sh
gh release delete v0.2.0-alpha.1 --yes --cleanup-tag   # only if nobody fetched it
```

Deleting tags that others may have fetched is hostile to `git describe` — prefer
cutting the next ordinal instead.

## Not configured

- **Nightly / dev builds.** There is no rolling build from `main`. If one is
  wanted, add a workflow that force-updates a single `dev` tag on every push.
- **Code signing.** The executable is unsigned, so Windows SmartScreen shows a
  warning on first launch. Signing would need a certificate and `signtool` in
  the build job.
- **Additional targets.** Only `x86_64-pc-windows-msvc` is built. 32-bit and
  ARM64 would each need a matrix entry and their own artifact suffix.
