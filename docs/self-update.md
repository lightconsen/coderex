# Self-update and release verification

`coderex update` installs the latest published release. `coderex update
--check` only reports whether one exists. The once-a-day startup check and the
`/update-check` command keep reporting; they never install anything.

```bash
coderex update --check     # "New version available: 0.2.0 (current: 0.1.0)"
coderex update             # download, verify, replace this binary
```

## What a release publishes

`.github/workflows/release.yml` builds one archive per target and attaches a
single `SHA256SUMS` to the release:

| target | asset |
|---|---|
| `x86_64-unknown-linux-gnu` | `coderex-x86_64-unknown-linux-gnu.tar.gz` |
| `x86_64-unknown-linux-musl` | `coderex-x86_64-unknown-linux-musl.tar.gz` |
| `x86_64-apple-darwin` | `coderex-x86_64-apple-darwin.tar.gz` |
| `aarch64-apple-darwin` | `coderex-aarch64-apple-darwin.tar.gz` |
| `x86_64-pc-windows-msvc` | `coderex-x86_64-pc-windows-msvc.exe` (and a `.zip` for manual installs) |

`SHA256SUMS` uses the standard `sha256sum` format — `<64 lowercase hex><two
spaces><asset name>` — so `sha256sum --check SHA256SUMS`, `scripts/install.sh`
and the updater all read the same file. The release job regenerates the file
after the build and the smoke job verifies it again before anything is
published.

The Windows updater downloads the bare `.exe`, not the `.zip`: a single-member
zip would mean taking on a zip decoder for no benefit. The `.zip` stays for
people who install by hand.

## What verification does and does not prove

The download is checked against `SHA256SUMS` before it replaces anything. That
proves the bytes match what the release published; it does **not** prove the
release is authentic. There is no signature, and the checksum file comes from
the same place as the archive, so a compromised release or account defeats
both. What the checksum does catch is a corrupt download, a truncated file and
a mirror that served something else.

`coderex update` never executes the downloaded binary before installing it.
The only step between the download and the rename is the checksum.

## Refusals

The updater refuses, with a message that names the reason, when:

- the release has no `SHA256SUMS`, or no entry for this platform's asset;
- the checksum does not match;
- the binary is owned by a package manager. Homebrew (`/opt/homebrew`,
  `/Cellar`), Nix (`/nix/store`) and `cargo install` (`~/.cargo/bin`) keep
  their own records; replacing the file would be undone by the next upgrade or
  leave the package database lying. The message names the command that
  upgrades it;
- the install directory is not writable by this user.

`--check` performs none of these refusals: it only reads the release document.

## How the replacement works

The download is staged in a temporary directory inside the binary's own
directory, so the final step is a rename that cannot cross a filesystem.

- **Unix**: the new file is made executable and renamed over the old one. The
  running process keeps the old inode, so replacing a running `coderex` is
  safe.
- **Windows**: a running executable cannot be replaced, but it can be renamed.
  The old file is moved to `coderex.exe.old`, the new one takes its place, and
  the rename is rolled back if that fails. `.old` is removed on the next
  launch (`main` calls `cleanup_after_update` before doing anything else).
- **macOS**: same as Unix. The release is unsigned today, so replacing the
  file does not invalidate a signature. If signing is added, the updater must
  either verify the downloaded signature or refuse to run on signed installs;
  that is a decision for whoever adds the certificates.

## Environment overrides

| Variable | Effect |
|---|---|
| `CODEREX_API_BASE_URL` | GitHub API base (`https://api.github.com/repos/lightconsen/coderex` by default). Point it at a mirror or a local server in tests. |
| `CODEREX_RELEASE_BASE_URL` | Asset base; when set, assets are read from `<base>/<tag>/<asset>` instead of the API's `browser_download_url`. |
| `CODEREX_DISABLE_UPDATE_CHECK` | `1`/`true`/`yes`/`on` stops the once-a-day startup check (and `should_check`). |

`scripts/install.sh` reads the same two base overrides and performs the same
checksum check; `scripts/install-smoke.sh` exercises both the successful path
and the two refusal paths (missing `SHA256SUMS`, wrong digest) against a fake
release on loopback.

## Tests

- `crates/cli/src/self_update.rs` — checksum parsing, target and asset naming,
  install-origin classification, tar extraction (path traversal, extra
  members), atomic replacement.
- `crates/cli/tests/e2e_self_update_binary.rs` — the real binary, copied to a
  scratch directory, updating itself from a fake release over loopback:
  `--check` leaves it alone, the normal path replaces and runs the new file, a
  bad checksum and a missing `SHA256SUMS` leave it untouched.
