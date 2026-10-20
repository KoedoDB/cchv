# Releasing

How we cut a release of KoedoDB's fork. (The original project's procedure is in its history; this file describes what the workflow
in this repository does today.)

## Versions

Tags look like **`v<minor line>-koedodb.<n>`**, for example `v0.16.0-koedodb.1`:

- the part before the dash continues the original's numbering (the original stopped at `v0.14.0`; our lines are `0.15` and `0.16`), so
  that a reader can see where the code comes from, and our tags cannot collide with a future tag of the original. **A feature release
  starts a new minor line** (`v0.16.0-koedodb.1` is the one with the disk-backed emptyDir and the 320 MiB default VM); fixes of a line
  count up in `-koedodb.<n>`;
- `-koedodb.<n>` counts our releases on top of it. It is a SemVer pre-release label on purpose: this is experimental software, and a tag
  with a dash is published as a **pre-release and never as "Latest"** (the workflow decides by the dash).
- `v0.15.0-koedodb.1` was the first release; it stays published as a pre-release and its notes say it is superseded. Never re-tag a
  published release.

The files do not carry the version (`Cargo.toml` says `0.1.0`); **the tag is the version**. The workflow reads it from the tag name and
writes it, with the commit, to `BUILD-INFO` before the build, so that `containerd-shim-cloudhv-v1 --version`,
`cloudhv-sandbox-daemon --version` and `cloudhv-agent --version` print the tag and the commit (and the pinned containerd-shimkit and
containerd-shim, and the Cloud Hypervisor, crun and kernel versions of `hacks/versions.env`). Outside a release, `crates/common/build.rs`
uses `git describe`. The workflow runs on tags matching `v<major>.<minor>.<patch>` and `v<major>.<minor>.<patch>-*`.

## Before you tag

1. `main` is what you want to release and is pushed. The working tree is clean (the untracked `guest/rootfs/rootfs-base/bin/crun` is not committed).
2. Tests pass: `cargo test -p cloudhv-sandbox-daemon --bins`, `cd crates/agent && cargo test`, and the shim and shimkit tests in Alpine
   (see [development.md](development.md); the host needs `protoc` for the shim crate, so the tests run in a container).
3. `cargo audit` in the root, in `crates/agent` and in the shimkit repository: no advisory.
4. Dependencies and pins are current (`hacks/versions.env`, `hacks/checksums.txt`; [supply-chain.md](supply-chain.md)); in the KoedoDB
   repository `scripts/check-advisories` reports nothing that needs action (Cloud Hypervisor, crun, the guest kernel, the crates).
5. The shim's `containerd-shimkit` dependency is pinned by commit to a tag of [KoedoDB/containerd-shimkit](https://github.com/KoedoDB/containerd-shimkit).
   [THIRD-PARTY.md](../THIRD-PARTY.md) names the versions of crun and the kernel with their source links: when `hacks/versions.env` changes
   those, change the file too (its source-offer links and the crun source hash).
6. An end-to-end run on a cluster (k3s with Cilium is what we use): pods start (also several at once), `kubectl exec`, `attach`, probes, ConfigMap/Secret/emptyDir,
   warm restore, deleting pods leaves no Cloud Hypervisor process or directory behind.

## Cutting the release

```bash
git tag -a v0.16.0-koedodb.1 -m "what this release is, in a sentence or two"
git push origin v0.16.0-koedodb.1
```

The push starts the **Release** workflow. If nothing starts: GitHub Actions must be enabled for this repository (a fork needs
a one-time confirmation in the *Actions* tab), and the organization must allow it.

## What the workflow does

0. Every job below that runs code is read-only. The first step of the build is `BUILD-INFO` (the tag and the commit, for `--version`).
1. **Build** (one job per architecture, x86_64 and aarch64, read-only token): the guest kernel (a pinned, checksum-verified kernel.org
   tarball, 6.18 LTS) and the guest root file system with the agent and a pinned `crun`, the shim and the daemon, and the Flatcar system
   extension. All downloads are checked against `hacks/checksums.txt`.
   **SBOMs and licenses** (one job, read-only): `cargo about` writes `THIRD-PARTY-LICENSES.txt` for the crates of the workspace and of
   `crates/agent` (`about.toml` lists the accepted licenses; a crate under another license fails the job), `cargo cyclonedx` writes one
   CycloneDX SBOM per shipped binary from `Cargo.lock`, and `hacks/gen-bundled-sbom.py` writes the SBOM of what is bundled and is not a
   crate (Cloud Hypervisor, CNI plugins, crun, the kernel, with the SHA-256 of what was downloaded). Both tools are installed from
   source at a pinned version with `--locked`; no binary is downloaded.
2. **Installer image** (needs `packages: write` and `id-token: write`): a multi-architecture image, pushed to GHCR as
   `ghcr.io/koedodb/cloudhv-installer:<tag>-amd64`, `-arm64`, and a manifest; the **digest** of the manifest is signed with cosign
   (keyless). There is **no `:latest`**: a tag that moves must not be what a privileged installer on every node runs.
3. **Release** (needs `contents: write`, `packages: write` and `id-token: write`; runs no code from the checkout): collects the artifacts
   (with the licenses, SBOMs and `THIRD-PARTY.md`), pushes the Helm chart to `oci://ghcr.io/koedodb/charts/cloudhv-installer`, **then**
   writes `checksums-sha256.txt` (so that it covers every file, the chart too) and signs it with cosign (keyless;
   `checksums-sha256.txt.sigstore.json`), writes the notes (with the GPL source offer), and creates the GitHub Release.

Cloud Hypervisor's version is the one in `hacks/versions.env` (v54.0 at the time of writing).

## What gets published

| Asset | |
|---|---|
| `containerd-shim-cloudhv-v1-linux-{amd64,arm64}`, `cloudhv-sandbox-daemon-linux-{amd64,arm64}`, `cloudhv-agent-linux-{amd64,arm64}` | host and guest binaries (`--version` prints the tag) |
| `vmlinux-{amd64,arm64}`, `vmlinux-{amd64,arm64}.kconfig`, `rootfs-{amd64,arm64}.erofs` | guest kernel (with its configuration) and root file system |
| `containerd-cloudhypervisor-<tag>-x86-64.raw` and `-arm64.raw` | Flatcar system extension |
| `cloudhv-installer-<version>.tgz` | Helm chart |
| `THIRD-PARTY-LICENSES.txt`, `THIRD-PARTY.md` | licenses of the crates; what is bundled, its licenses and the source of the GPL parts |
| `sbom-containerd-shim-cloudhv-v1.cdx.json`, `sbom-cloudhv-sandbox-daemon.cdx.json`, `sbom-cloudhv-agent.cdx.json`, `sbom-bundled-components.cdx.json` | CycloneDX SBOMs |
| `checksums-sha256.txt`, `checksums-sha256.txt.sigstore.json` | SHA-256 of all of the above, and its keyless signature |

## Verifying and installing a release

```bash
TAG=v0.17.0-koedodb.1; BASE=https://github.com/KoedoDB/cchv/releases/download/$TAG
wget "$BASE/checksums-sha256.txt" "$BASE/checksums-sha256.txt.sigstore.json" "$BASE/containerd-shim-cloudhv-v1-linux-amd64"
cosign verify-blob --bundle checksums-sha256.txt.sigstore.json \
  --certificate-identity-regexp '^https://github.com/KoedoDB/cchv/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com checksums-sha256.txt
sha256sum -c checksums-sha256.txt --ignore-missing
chmod +x containerd-shim-cloudhv-v1-linux-amd64 && ./containerd-shim-cloudhv-v1-linux-amd64 --version
```

The installer image is signed; check it by its digest (the release notes give it):

```bash
cosign verify ghcr.io/koedodb/cloudhv-installer@sha256:<digest> \
  --certificate-identity-regexp '^https://github.com/KoedoDB/cchv/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

**Releases up to `v0.16.0-koedodb.1` were signed when the repository was called `containerd-cloudhypervisor`**: for them the identity is
`^https://github.com/KoedoDB/containerd-cloudhypervisor/` (the name is in the certificate).

For the Helm chart, always pass the image digest (`--set image.digest=sha256:<digest>`); the chart refuses to render without a digest or a tag.
Remember that the installer edits `/etc/containerd/config.toml`, which suits plain containerd hosts; k3s needs its template instead
(see the README's examples). Configuration of the shim and the daemon: [configuration.md](configuration.md); the daemon's
`ch_rootless` mode and what it needs: [sandbox-daemon.md](sandbox-daemon.md).

## After the release

- Check the release page and the assets, `cosign verify` on the digest, `cosign verify-blob` on the checksums file, and `--version` of a downloaded
  binary (it must print the tag).
- Note the release, and what was tested, in the KoedoDB repository (`doc/LAMBDA.md`).
- **Not yet exercised end to end**: the signing of the installer image and of the checksums file, the Helm push, and the job that makes the licenses and
  SBOMs have never run on a real tag (the notes in the workflow say so; the steps' scripts were run locally on fake artifacts, and `cargo about` and
  `cargo cyclonedx` on the workspace). The first release that works is the proof, and this file should then lose this paragraph.

## GPL components

The guest root file system contains crun (GPL-2.0-or-later) and the guest kernel is Linux (GPL-2.0-only). Every release's notes carry the written
source offer, and [THIRD-PARTY.md](../THIRD-PARTY.md) has the links and hashes. If crun or the kernel version changes, update that file in the same
commit, and read the new version's license file (crun has `COPYING` and `COPYING.libcrun`).
