# Supply chain: what is pinned, how it is checked, how to update it

Everything this repository builds or deploys comes from somewhere else (base images, release binaries, a kernel tarball, crates,
GitHub Actions). The installer image runs **privileged on every node**, and the guest kernel and rootfs run customers' code,
so each input is pinned to something that cannot change under us, and checked against what its publisher says.

## What is pinned, and where

| Input | Pinned as | Where |
|---|---|---|
| Build container (Alpine) | image digest | `hacks/versions.env` (`ALPINE_IMAGE`) |
| Installer image bases (`rust`, `debian`) | image digest | `installer/Dockerfile`, `installer/smoke-test.sh` |
| Pause image of the DaemonSet | image digest | `charts/cloudhv-installer/values.yaml` |
| Cloud Hypervisor (sysext binary) | version + SHA-256 | `versions.env`, `hacks/checksums.txt` |
| Cloud Hypervisor (installer image, CI) | tag + commit (built from source, commit checked) | `versions.env` (`CH_COMMIT`), `installer/Dockerfile` |
| CNI plugins | version + SHA-256 | `versions.env`, `checksums.txt` |
| Guest kernel | exact version + SHA-256 (never "latest") | `versions.env`, `checksums.txt` |
| crun in the guest rootfs | version + SHA-256 | `versions.env`, `checksums.txt` |
| erofs-utils (`mkfs.erofs`) | tag + commit (checked after the clone) | `versions.env` |
| Flatcar sysext-bakery (CI test) | commit | `versions.env` |
| Rust crates | `Cargo.lock` (with checksums); every build uses `--locked` | `Cargo.lock`, `crates/agent/Cargo.lock` |
| GitHub Actions | full commit SHA (the tag in a comment) | `.github/workflows/*.yml` |
| Release installer image | digest, signed (cosign keyless) | the release notes and the Helm chart name it by digest |

A download whose name is not in `hacks/checksums.txt`, or whose SHA-256 differs, **fails the build** (`hacks/verify-download.sh`).

## Where each checksum comes from

`bash hacks/check-upstream.sh` (run in CI, `GITHUB_TOKEN` avoids GitHub's anonymous API limit) repeats these checks:

| Entry | Checked against | Trust |
|---|---|---|
| `cloud-hypervisor-<v>-static[-aarch64]` | the `digest` GitHub publishes for the release asset | upstream-published (via GitHub's API) |
| `cni-plugins-linux-<arch>-<v>.tgz` | the `.sha256` file in the same release | upstream-published (same release, so same trust as the asset) |
| `linux-<v>.tar.xz` | kernel.org `sha256sums.asc`, whose signature is checked with the kernel.org checksum autosigner key (found by WKD) | signed by kernel.org |
| `crun-<v>-linux-<arch>-disable-systemd` | the release's detached `.asc`, verified with the maintainer's key (`AC40 4C1C 0BF7 35C6 3FF4 D562 263D 6DF2 E163 E1EA`) | signed; **the key was fetched from a key server: trust on first use** |
| `CH_COMMIT`, `EROFS_UTILS_COMMIT` | `git ls-remote <repo> 'refs/tags/<tag>^{}'` | the tag as the upstream repository has it now |

The first value of each entry was computed by downloading the file once and was equal to the upstream value on 2026-10-16. crun was
raised from 1.20 to 1.30.1 on 2026-10-16: both `.asc` signatures verified (key `AC40 4C1C 0BF7 35C6 3FF4 D562 263D 6DF2 E163 E1EA`, signing
subkey `AF60 FCA3 CDAA 6DEA D157 EA3A 67E3 8F7A 8BA2 1772`), the amd64 binary was downloaded twice with the same hash, both are static ELF.
What remains trust-on-first-use: the crun signing key (its fingerprint is recorded in `checksums.txt`, and the script fails if
the signature stops verifying), and the fact that GitHub's digests and the `.sha256` files are published by the same account
that publishes the binaries.

## Updating a pin

1. Read the new release's notes and its security advisories first (`scripts/check-advisories` in the main repository covers some).
2. Change the version in `hacks/versions.env` (and the commit, where there is one: `git ls-remote <repo> 'refs/tags/<tag>^{}'`).
3. Replace the entries in `hacks/checksums.txt`: download the file, `sha256sum` it, and make `bash hacks/check-upstream.sh` pass.
   If it cannot be checked against upstream, say so in the comment at the top of `checksums.txt`.
4. Images: `docker buildx imagetools inspect <image:tag>` gives the digest of the index; put `name:tag@sha256:...` everywhere that
   image appears (`versions.env`, `installer/Dockerfile`, `installer/smoke-test.sh`, `values.yaml`).
5. Actions: `git ls-remote https://github.com/<owner>/<repo>.git 'refs/tags/<tag>' 'refs/tags/<tag>^{}'`; use the `^{}` line if
   there is one (an annotated tag), else the first. Keep the tag in the comment after the SHA.
6. Crates: `cargo update -p <crate>` and commit `Cargo.lock`; never build without `--locked`.

## CI and release

- `permissions: {}` at the top of the release workflow, and per job only what it needs. The jobs that run build code (every crate's
  `build.rs`, the kernel's build system) can only read. The one job that writes (GitHub release, Helm chart) runs no code from the
  checkout. Every `actions/checkout` has `persist-credentials: false`, so no token is left in `.git/config`.
- `hacks/util.inc` (`docker_wrapper`) runs builds in the pinned Alpine image with only the capabilities `apk add` and the final
  `chown` need (`--cap-drop ALL`, `no-new-privileges`) and an empty tmpfs over the checkout's `.git`. The container is still root inside
  and has the network (crates, apk). The checkout is mounted read-write because the artifacts are written back to it.
- The installer image is built by BuildKit from `installer/Dockerfile` (it builds Cloud Hypervisor from source there, isolated from the
  runner). It is pushed per architecture, combined into one manifest **without `:latest`**, and signed with cosign (keyless, GitHub's
  OIDC identity). The Helm chart of the release and the release notes name the image by digest. The chart refuses to render without
  a digest or a tag.
  Verify: `cosign verify --certificate-identity-regexp '^https://github.com/<owner>/<repo>/' --certificate-oidc-issuer https://token.actions.githubusercontent.com ghcr.io/<owner>/cloudhv-installer@<digest>`
- **Review before the first tag:** the signing step and the digest handling in `release.yml` have not run yet (they need a real tag push).

## The installer on the node

`installer/install.sh` runs in an **init container** (privileged, host PID); after it exits, the pod keeps only an unprivileged pause
container (`runAsNonRoot`, read-only root, no capabilities, no service account token). It checks the artifacts before it touches
anything, copies each file beside its final name, compares and renames it, uses a `mktemp -d` directory on the node, and does not
change containerd's config (nor restart containerd) unless the shim, daemon, Cloud Hypervisor and guest files are installed and
Cloud Hypervisor runs. `installer/smoke-test.sh` exercises that on a stubbed node (CI runs it).
The daemon's unit has a capability bounding set, `ProtectSystem=strict`, a device allow list and an address family allow list; see the
comments in `sysext/root/usr/lib/systemd/system/cloudhv-sandbox-daemon.service`. **Not yet tested under systemd on a real node.**

## What is not done

- `apk add` (build containers) and `apt-get install` (installer image) take the version of the pinned distribution release's
  repository (signed index); packages are not pinned one by one, because old builds disappear from the repositories.
- The Flatcar image that `hacks/test-sysext.sh` boots is downloaded by the bakery and is not checked by us (CI test only).
- `containerd-shimkit` is a git dependency on a **branch** of a personal fork (`crates/shim/Cargo.toml`); `Cargo.lock` fixes the commit,
  but `rev = "<commit>"` in `Cargo.toml` would make that explicit and stop an accidental `cargo update` from moving it.
- No SBOM, no signature on the release binaries (only on the installer image), no `cargo audit`/`cargo vet` in CI.
- The workspace `Cargo.lock` was out of date when `--locked` was introduced (`containerd-shim-cloudhv` depends on `sha2`, the lock entry
  does not list it): run `cargo metadata` once and commit the result, or every `--locked` build fails.
- `guest/kernel/build-kernel.sh` and `guest/rootfs/build-rootfs.sh` still download the kernel and crun with a bare `wget`. Until they
  use `download_verified`, `hacks/build-guest-kernel.sh` unpacks a checked kernel tarball first (so the script does not download), and
  `hacks/build-guest.sh` checks the crun it installed against `checksums.txt` before the rootfs leaves the build container.
  Builds that call those two scripts directly (`make build-kernel`, `make build-rootfs`) are not covered.
