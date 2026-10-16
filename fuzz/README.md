# Fuzzing

Fuzz targets (cargo-fuzz / libFuzzer) for the code that interprets input an attacker can influence: the OCI spec the host sends the guest
agent, the requests to the daemon, the files in volumes and snapshots, the task calls containerd makes. Each target runs the production
code on what the fuzzer made and asserts what the code promises: that nothing panics, hangs or runs out of memory, and the properties
below. A failure is either a bug or a promise the code does not keep.

| Target | Input | What it runs | What must hold |
|---|---|---|---|
| `agent_spec` | JSON | `sanitize_spec`, `sanitize_exec_process`, `seccomp::enforce`, `sanitize_resources`, capability filters | only the allowed capabilities, in every set, and an exec never has more than its container; `noNewPrivileges`; only the allowed spec fields; the device rules; the seccomp profile has a `defaultAction`, no listener, allows no refused syscall, and cannot allow `socket(AF_VSOCK)` |
| `agent_small` | text | destination, hostname, id, volume name checks, `mounts_under`, disk helpers | no destination in `/proc` or `/sys` however it is written; ids and names are plain; deepest mounts first |
| `agent_proto` | protobuf bytes | every RPC request message | decoding does not panic, and what is written parses back equal |
| `agent_logs` | operations | the bounded output buffer | bounded, byte accounting exact, offsets never beyond the stream |
| `agent_volumes` | a volume and what a container planted in it | `volumes::materialize` | nothing outside the volume root is read, written or created (links, hard links, FIFOs); the path it returns is below the root |
| `daemon_validate` | text / JSON | id, netns, erofs path, size and snapshot-configuration checks | what is accepted is plain and in the right place; an accepted snapshot configuration has the pool's CPUs and memory, only the allowed disks, read-only, and no device that reaches the host |
| `daemon_request` | JSON | the id fields and `volumes` of a request | an id that passes is an id; bounded |
| `daemon_ctl` | bytes | `/proc` text and command-line parsers used before a signal is sent | no panic; the parsers agree with what they promise (no zombie counted; `--api-socket` exact) |
| `daemon_tree` | a tree a VM made | `hand_over_snapshot`, `remove_entry` | nothing outside the tree changes owner, mode or content; what is kept is a plain file or directory, handed over; no hang; deep trees are refused, not walked |
| `shim_volumes` | a tree and an OCI spec | `volumes::from_spec` | no file outside the volume roots is read, whatever links the tree has; the guest path is `/run/cloudhv-vols/<name>` |
| `shim_small` | bytes | digest, cgroups path, `vm_size`, the limits file | a cgroups path has no empty, `.` or `..` part; a VM has at least one CPU and the default memory; no overflow |
| `shimkit_task` | calls (4 bytes each) | the task service (`Local`) with an instance that keeps books | no panic or hang; an exec's events come in order; a deleted exec is gone; `Wait` before `Start` does not fail |

## Running

The targets need a nightly Rust and `cargo-fuzz`, so everything runs in a container (`Dockerfile`; build it once):

```
docker build -t cchv-fuzz fuzz
```

The sources fuzzed are a **snapshot** of the committed code (so that work in progress in the trees does not stop the build):

```
fuzz/snapshot.sh                 # the repository at HEAD, and the shimkit commit its Cargo.toml pins, into ~/.cchv-fuzz-src
export FUZZ_SRC=$HOME/.cchv-fuzz-src
fuzz/in-docker.sh cargo fuzz build -O
fuzz/run.sh agent_spec 600       # one target, 600 seconds, one core
fuzz/run-batch.sh 600 agent_spec daemon_tree shim_volumes shimkit_task   # several at once, one core each; logs in fuzz/logs/
fuzz/run-all.sh 600              # every target, four at a time
```

(The snapshot must be under the home directory: a snap-installed Docker cannot mount `/tmp`.) A few functions are made `pub`/`pub(crate)`
in the production sources so that the entry points can call them; `snapshot.sh` does the same to the snapshot, and the shimkit fuzz module
(`shimkit/fuzz.rs`, built only with `--cfg fuzzing`) is added to the shimkit snapshot. After a fix in the working tree, copy the file into
the snapshot with `fuzz/sync.sh crates/agent/src/spec.rs`.

`FUZZ_CPUS` (default 4 for `in-docker.sh`, 1 for `run.sh`) limits the cores a container may use.

## A crash

libFuzzer writes the input to `fuzz/artifacts/<target>/crash-*` (or `timeout-*`, `oom-*`). Run it again, with a backtrace:

```
RUST_BACKTRACE=1 fuzz/in-docker.sh cargo fuzz run -O agent_spec artifacts/agent_spec/crash-...
```

Make the input a regression test next to the code it broke, fix the code, and run the target again for a few minutes. The corpora
(`fuzz/corpus/`) and artifacts are not tracked; only `seeds/` (a few valid inputs per target) and `dict/` (JSON keywords) are. The
container runs as root, so the files it writes are root's: remove them with `docker run --rm -v "$PWD/fuzz":/w alpine rm -rf /w/artifacts /w/corpus`.

## Not covered

The code that does I/O on the system as a whole (mounting, `crun`, the guest's PID 1), the ttrpc and JSON socket servers themselves, the
shim's pumps (`exec.rs`), and `erofs_cache` (it runs `mkfs.erofs`). Their parsing and validation parts are the targets above; the rest is
covered by the unit tests and the end-to-end runs. Coverage numbers at the end of a run show whether a longer run is worth it: while
`ft:` still grows, it is.
