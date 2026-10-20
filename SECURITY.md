# Security policy

## Reporting a vulnerability

Please report security problems **privately**, not as a public issue:

- by email to **security@koedodb.com**, or
- with GitHub's private reporting: **Security → Report a vulnerability** on this repository.

Include what you found, how to reproduce it (a proof of concept helps), and the version or commit you tested. We will acknowledge
your report, look into it, and tell you what we decided. This is a small project with a single maintainer, so please allow a few
days for a first answer; we cannot promise a fixed response time or offer a bounty.

## What this repository is

KoedoDB's fork of [devigned/containerd-cloudhypervisor](https://github.com/devigned/containerd-cloudhypervisor): a containerd shim, a
sandbox daemon and a guest agent that run Pods in Cloud Hypervisor microVMs. The shim and the daemon run as root on a node; the agent
runs as PID 1 in the guest and starts untrusted containers. Flaws in them matter, and we want to hear about them. The README says what
the project is and how far it has been checked: **it is experimental and has not been reviewed from outside**.

- **In scope**: the code in this repository: the shim, the daemon (including its confinement of Cloud Hypervisor), the guest agent
  and the guest kernel configuration, the installer and the build and release scripts.
- **Not in scope**: flaws in Cloud Hypervisor (report them to [cloud-hypervisor/cloud-hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor/security)),
  in `crun`, containerd, the Linux kernel and other dependencies (report them there; tell us too if we should update), and flaws in code of
  the original project that we kept unchanged (report them to the original; we will take the fix over). Our `containerd-shimkit` has its
  own repository and policy: [KoedoDB/containerd-shimkit](https://github.com/KoedoDB/containerd-shimkit/security/policy).

## Supported versions

Only the latest tag (`v0.17.0-koedodb.N`) is supported.

## How we look for problems

Everything is read and reviewed by us, with AI assistance; the input parsers are fuzzed (`fuzz/`); `cargo audit` and the advisories of
Cloud Hypervisor, crun and the guest kernel are checked when we work on it (see `docs/supply-chain.md`). 
