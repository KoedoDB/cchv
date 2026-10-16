#!/bin/sh
# Make a snapshot of the committed sources to fuzz, away from whatever is being edited in the working trees:
#   fuzz/snapshot.sh [REV] [SHIMKIT_REV]      (defaults: HEAD, and the shimkit commit the shim's Cargo.toml pins)
# It writes $HOME/.cchv-fuzz-src/cch (the repository at REV) and $HOME/.cchv-fuzz-src/shimkit (shimkit at SHIMKIT_REV), then makes the few
# functions the entry points call visible (pub(crate)/pub; harmless, and already so in the working tree) and adds the shimkit fuzz module.
# in-docker.sh uses the snapshot when FUZZ_SRC=$HOME/.cchv-fuzz-src is set.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SHIMKIT_REPO=${SHIMKIT_DIR:-$ROOT/../containerd-shimkit}
REV=${1:-HEAD}
OUT=${FUZZ_SRC:-$HOME/.cchv-fuzz-src}
PIN=${2:-$(git -C "$ROOT" show "$REV:crates/shim/Cargo.toml" | sed -n 's/.*containerd-shimkit.*rev = "\([0-9a-f]*\)".*/\1/p')}
rm -rf "$OUT"; mkdir -p "$OUT/cch" "$OUT/shimkit"
git -C "$ROOT" archive "$REV" | tar -x -C "$OUT/cch"
git -C "$SHIMKIT_REPO" archive "$PIN" | tar -x -C "$OUT/shimkit"
# visibility, as in the working tree
sed -i 's/^fn netns_path_allowed_with(/pub(crate) fn netns_path_allowed_with(/' "$OUT/cch/crates/daemon/src/validate.rs"
sed -i 's/^fn parse_status_uid(/pub(crate) fn parse_status_uid(/; s/^fn status_has_uid(/pub(crate) fn status_has_uid(/' "$OUT/cch/crates/daemon/src/ctl.rs"
sed -i 's/^fn required_id</pub(crate) fn required_id</; s/^fn volumes_from_request(/pub(crate) fn volumes_from_request(/' "$OUT/cch/crates/daemon/src/api.rs"
sed -i 's/^fn valid_cgroups_path(/pub fn valid_cgroups_path(/' "$OUT/cch/crates/shim/src/instance.rs"
# the shimkit fuzz module
cp "$ROOT/fuzz/shimkit/fuzz.rs" "$OUT/shimkit/src/sandbox/shim/local/fuzz.rs"
grep -q 'pub mod fuzz;' "$OUT/shimkit/src/sandbox/shim/local.rs" || \
  sed -i 's/^mod tests;$/mod tests;\n\n#[cfg(fuzzing)]\npub mod fuzz;/' "$OUT/shimkit/src/sandbox/shim/local.rs"
grep -q 'fuzz_entry' "$OUT/shimkit/src/sandbox/shim/mod.rs" || \
  sed -i 's/^mod local;$/mod local;\n#[cfg(fuzzing)]\npub use local::fuzz as fuzz_entry;/' "$OUT/shimkit/src/sandbox/shim/mod.rs"
echo "snapshot of $REV (shimkit $PIN) in $OUT"
