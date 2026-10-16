#!/usr/bin/env python3
"""A CycloneDX SBOM of what a release bundles that is not a Rust crate: Cloud Hypervisor, the CNI plugins, crun and the guest kernel.

The versions come from hacks/versions.env and the SHA-256 of what is downloaded from hacks/checksums.txt, so the SBOM says what the
build was checked against. (The Rust crates have their own SBOMs, made by cargo-cyclonedx from Cargo.lock.) Only the standard
library is used. Usage: gen-bundled-sbom.py <tag> <output.json>
"""
import json
import re
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def pins():
    out = {}
    for line in (ROOT / "hacks/versions.env").read_text().splitlines():
        m = re.search(r"\$\{(\w+):=([^}]*)\}", line)
        if m:
            out[m.group(1)] = m.group(2)
    return out


def checksums():
    out = {}
    for line in (ROOT / "hacks/checksums.txt").read_text().splitlines():
        if line.startswith("#") or not line.strip():
            continue
        digest, name = line.split(None, 1)
        out[name.strip()] = digest
    return out


def component(name, version, purl, licenses, url, files, desc):
    c = {
        "type": "application",
        "name": name,
        "version": version,
        "description": desc,
        "purl": purl,
        "licenses": [{"expression": licenses}],
        "externalReferences": [{"type": "distribution", "url": url}],
    }
    hashes = [{"alg": "SHA-256", "content": d} for d in files if d]
    if hashes:
        c["hashes"] = hashes
    return c


def main():
    tag, dest = sys.argv[1], sys.argv[2]
    p, sums = pins(), checksums()
    ch, cni, crun, kernel = p["CH_VERSION"], p["CNI_VERSION"], p["CRUN_VERSION"], p["KERNEL_VERSION"]
    comps = [
        component(
            "cloud-hypervisor", ch, f"pkg:github/cloud-hypervisor/cloud-hypervisor@{ch}", "Apache-2.0 AND BSD-3-Clause",
            f"https://github.com/cloud-hypervisor/cloud-hypervisor/releases/tag/{ch}",
            [sums.get(f"cloud-hypervisor-{ch}-static"), sums.get(f"cloud-hypervisor-{ch}-static-aarch64")],
            "The virtual machine monitor in the system extension (the release's static binaries: x86_64, then aarch64). "
            "Source files carry their own SPDX license (the main program is Apache-2.0, parts are BSD-3-Clause: LICENSES/ in the "
            "repository). The installer image builds the same tag from source."),
        component(
            "containernetworking-plugins", cni, f"pkg:github/containernetworking/plugins@{cni}", "Apache-2.0",
            f"https://github.com/containernetworking/plugins/releases/tag/{cni}",
            [sums.get(f"cni-plugins-linux-amd64-{cni}.tgz"), sums.get(f"cni-plugins-linux-arm64-{cni}.tgz")],
            "CNI plugins in the system extension (amd64, then arm64)."),
        component(
            "crun", crun, f"pkg:github/containers/crun@{crun}", "GPL-2.0-or-later",
            f"https://github.com/containers/crun/releases/tag/{crun}",
            [sums.get(f"crun-{crun}-linux-amd64-disable-systemd"), sums.get(f"crun-{crun}-linux-arm64-disable-systemd")],
            "The OCI runtime in the guest root file system (static, disable-systemd build; amd64, then arm64). crun is GPL-2.0-or-later "
            "(COPYING); its libcrun is under COPYING.libcrun (LGPL-2.1). The static binary contains libraries of its own that this SBOM "
            "does not itemize."),
        component(
            "linux", kernel, f"pkg:generic/linux@{kernel}?download_url=https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-{kernel}.tar.xz",
            "GPL-2.0-only WITH Linux-syscall-note",
            f"https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-{kernel}.tar.xz",
            [sums.get(f"linux-{kernel}.tar.xz")],
            "The guest kernel (vmlinux in the release is built from this source tarball with guest/kernel/configs/microvm.config; "
            "the hash is the source tarball's)."),
    ]
    for c in comps:
        if not c.get("hashes"):
            sys.exit(f"no checksum in hacks/checksums.txt for {c['name']} {c['version']}")
    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid4()}",
        "version": 1,
        "metadata": {
            "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "component": {"type": "application", "name": "containerd-cloudhypervisor", "version": tag,
                          "description": "KoedoDB's fork: components the release bundles besides the Rust crates"},
            "tools": [{"name": "hacks/gen-bundled-sbom.py"}],
        },
        "components": comps,
    }
    Path(dest).write_text(json.dumps(bom, indent=2) + "\n")


if __name__ == "__main__":
    main()
