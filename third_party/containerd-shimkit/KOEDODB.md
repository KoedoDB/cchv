# containerd-shimkit, as used by containerd-cloudhypervisor

This directory started as a copy of the `containerd-shimkit` crate of [containerd/runwasi](https://github.com/containerd/runwasi)
(Apache-2.0, see `LICENSE`), and has been cut down and extended for one use: a shim whose containers run in a VM. The shim
(`crates/shim`) depends on it by path.

## Where it comes from

- **Base**: `crates/containerd-shimkit` of containerd/runwasi at the tag `containerd-shimkit/v0.1.2`, commit `5a8fc8e9` (3,436 source
  lines, tests included).
- **Patches taken from `devigned/runwasi`, branch `shimkit-ttrpc-0.10`** (the personal fork this project used before):
  `ee3b570` allow `TaskState` signal from the `Exited` state; `40679f4` allow an idempotent delete in `TaskState`; `faee81b` return
  empty metrics with the correct cgroup v1/v2 type URL. (Its libcontainer changes are superseded by the removal below.)
- **`containerd-shim` 0.11** (upstream: 0.8): `containerd-shim-protos` 0.11 (ttrpc 0.9, protobuf 3.7), nix 0.31, oci-spec 0.9.
- **Exec** (`src/sandbox/shim/exec.rs`, `instance_data.rs`, `local.rs`, `instance.rs`): the task service implements `Exec`, `Start`,
  `Kill`, `Wait`, `Delete`, `State`, `ResizePty` and `CloseIO` for exec ids, with an exec table per task and a state machine, and
  publishes `TaskExecAdded`, `TaskExecStarted` and `TaskExit` for them. The `Instance` trait got optional methods (`exec_*`, default:
  unimplemented) and `ExecConfig`. Upstream refuses every call with an exec id.
- **Terminals**: `Create` accepts `terminal` (upstream refuses it) and passes it in `InstanceConfig.terminal`; `ResizePty` and `CloseIO`
  without an exec id go to the new optional `Instance::resize_pty` and `Instance::close_stdin`; `State` reports `terminal`.

## What was removed, and why

| Removed | Why |
|---|---|
| **zygote** (a helper process forked at start-up, for the logger and a memory log line) | It was the cause of the `start` hang with containerd-shim 0.11: `start` reads the server's stdout pipe to the end, and the helper, forked while fd 1 was that pipe, kept its write end open. Redundant too: `containerd_shim::run` sets up the logger itself in the server. |
| `libcontainer`, `dbus`, `Error::Libcontainer` | A shim that runs containers in VMs does not use youki. As an optional dependency it still conflicted in the lock file (`libcontainer` pins `protobuf =3.2.0`). |
| **OpenTelemetry and tracing** (`otel.rs`, the `opentelemetry` and `tracing` features and every `#[cfg_attr(feature = "tracing", ...)]`) | Not used; hundreds of lines and a dozen crates. |
| **Windows** (`sys/windows`, the vendored windows logger, `windows-sys`, `mio`) | The shim runs on Linux only. |
| The **vendored `containerd-shim` logger** | Unused: `containerd_shim::run` has its own. |
| `oci.rs`: **prestart hooks** run by the shim at Create | Running host programs named in a bundle's spec, as root, is attack surface the VM shim does not need (the guest agent drops hooks too). |
| `Config` / runtime options parsing (`prost`, `toml`), `determine_rootdir`, the `version!`/`revision!` macros (`git-version`) | Wasm-engine options and paths; not used. |
| `caps`, `containerd-client`, `serde_bytes`, `tempfile` (non-test), `time`, `serde` derives | Not used by the crate (`containerd-client` is a dependency of the shim itself, for the image digest). |
| `futures` default features, `tokio-async-drop` (now test only) | Smaller dependency tree. |
| The two tests of runtime-options parsing, the wasm-oriented docs | Went with the code they tested. |

Kept: the `Local` task service, events, `TaskState`, `WaitableCell`, `AmbientRuntime`, the exec state machine, `shim_main`.

## Numbers

Source lines of `src/` (tests included): 3,436 in upstream v0.1.2; 4,348 after the exec work and before the cut; 2,875 now, exec
included (tests are 867 of them). The static shim binary went from 4.66 MB to 4.37 MB. Direct
dependencies: 22 → 11 (+3 for tests). Crates in the shim's dependency tree: 181 with the previous git shimkit and `containerd-shim`
0.8, 217 with v0.1.2 and 0.11 before the cut, 204 now. Most of the rise is `containerd-shim` 0.11 itself: its `cgroups-rs` 0.5
depends unconditionally on `zbus` (D-Bus), about 35 crates this shim never uses. That can only go by patching `containerd-shim`
(see below).

## Updating

To take a newer upstream version: copy `crates/containerd-shimkit` of the new tag, then redo the removals above and re-apply the
exec changes (`git log -- third_party/containerd-shimkit` has them as commits). `cargo test -p containerd-shimkit` (with `protoc`
installed, for `containerd-client`) covers the task service and the exec state machine. `crates/shim/tests/start_handshake.sh`
checks `start` with the real binary.

## Possible next cut

`[patch.crates-io]` a vendored copy of `containerd-shim` 0.11 without its `cgroup` module (the shim does not use it): drops
`cgroups-rs` and `zbus` and about 35 crates. Not done.
