# Releasing

How we cut a release of KoedoDB's fork. (The original project's procedure is in its history; this file describes what the workflow
in this repository does today.)

## Versions

Tags look like **`v<upstream version we started from>-koedodb.<n>`**, for example `v0.15.0-koedodb.1`:

- the part before the dash follows the original's numbering (the original stopped at `v0.14.0`), so that a reader can see where the code
  comes from, and our tags cannot collide with a future tag of the original;
- `-koedodb.<n>` counts our releases on top of it. It is a SemVer pre-release label on purpose: this is experimental software.

The files do not carry the version (`Cargo.toml` says `0.1.0`); **the tag is the version**. The workflow reads it from the tag name.
It runs on tags matching `v<major>.<minor>.<patch>` and `v<major>.<minor>.<patch>-*`.

## Before you tag

1. `main` is what you want to release and is pushed. The working tree is clean (the untracked `guest/rootfs/rootfs-base/bin/crun` is not committed).
2. Tests pass: `cargo test -p cloudhv-sandbox-daemon --bins`, `cd crates/agent && cargo test`, and the shim and shimkit tests in Alpine
   (see [development.md](development.md); the host needs `protoc` for the shim crate, so the tests run in a container).
3. `cargo audit` in the root, in `crates/agent` and in the shimkit repository: no advisory.
4. Dependencies and pins are current (`hacks/versions.env`, `hacks/checksums.txt`; [supply-chain.md](supply-chain.md)); in the KoedoDB
   repository `scripts/check-advisories` reports nothing that needs action (Cloud Hypervisor, crun, the guest kernel, the crates).
5. The shim's `containerd-shimkit` dependency is pinned by commit to a tag of [KoedoDB/containerd-shimkit](https://github.com/KoedoDB/containerd-shimkit).
6. An end-to-end run on a cluster (k3s with Cilium is what we use): pods start (also several at once), `kubectl exec`, `attach`, probes, ConfigMap/Secret/emptyDir,
   warm restore, deleting pods leaves no Cloud Hypervisor process or directory behind.

## Cutting the release

```bash
git tag -a v0.15.0-koedodb.2 -m "what this release is, in a sentence or two"
git push origin v0.15.0-koedodb.2
```

The push starts the **Release** workflow. If nothing starts: GitHub Actions must be enabled for this repository (a fork needs
a one-time confirmation in the *Actions* tab), and the organization must allow it.

## What the workflow does

1. **Build** (one job per architecture, x86_64 and aarch64, read-only token): the guest kernel (a pinned, checksum-verified kernel.org
   tarball, 6.18 LTS) and the guest root file system with the agent and a pinned `crun`, the shim and the daemon, and the Flatcar system
   extension. All downloads are checked against `hacks/checksums.txt`.
2. **Installer image** (needs `packages: write` and `id-token: write`): a multi-architecture image, pushed to GHCR as
   `ghcr.io/koedodb/cloudhv-installer:<tag>-amd64`, `-arm64`, and a manifest; the **digest** of the manifest is signed with cosign
   (keyless). There is **no `:latest`**: a tag that moves must not be what a privileged installer on every node runs.
3. **Release** (needs `contents: write`; runs no code from the checkout): collects the artifacts, writes `checksums-sha256.txt`,
   pushes the Helm chart to `oci://ghcr.io/koedodb/charts/cloudhv-installer`, writes the notes, and creates the GitHub Release.

Cloud Hypervisor's version is the one in `hacks/versions.env` (v54.0 at the time of writing).

## What gets published

| Asset | |
|---|---|
| `containerd-shim-cloudhv-v1-linux-{amd64,arm64}`, `cloudhv-sandbox-daemon-linux-{amd64,arm64}`, `cloudhv-agent-linux-{amd64,arm64}` | host and guest binaries |
| `vmlinux`, `vmlinux-arm64`, `rootfs.erofs`, `rootfs-arm64.ext4` | guest kernel and root file system |
| `containerd-cloudhypervisor-<tag>-x86-64.raw` and the arm64 one | Flatcar system extension |
| `cloudhv-installer-chart-<version>.tgz` | Helm chart |
| `checksums-sha256.txt` | SHA-256 of all of the above |

## Verifying and installing a release

```bash
TAG=v0.15.0-koedodb.1; BASE=https://github.com/KoedoDB/containerd-cloudhypervisor/releases/download/$TAG
wget "$BASE/checksums-sha256.txt" "$BASE/containerd-shim-cloudhv-v1-linux-amd64"
sha256sum -c checksums-sha256.txt --ignore-missing
```

The installer image is signed; check it by its digest (the release notes give it):

```bash
cosign verify ghcr.io/koedodb/cloudhv-installer@sha256:<digest> \
  --certificate-identity-regexp '^https://github.com/KoedoDB/containerd-cloudhypervisor/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

For the Helm chart, always pass the image digest (`--set image.digest=sha256:<digest>`); the chart refuses to render without a digest or a tag.
Remember that the installer edits `/etc/containerd/config.toml`, which suits plain containerd hosts; k3s needs its template instead
(see the README's examples). Configuration of the shim and the daemon: [configuration.md](configuration.md); the daemon's
`ch_rootless` mode and what it needs: [sandbox-daemon.md](sandbox-daemon.md).

## After the release

- Check the release page and the assets, and `cosign verify` on the digest.
- Note the release, and what was tested, in the KoedoDB repository (`doc/LAMBDA.md`).
- **Not yet exercised end to end**: the signing step and the Helm push have never run on a real tag (the notes in the workflow say so); the first release
  that works is the proof, and this file should then lose this paragraph.
