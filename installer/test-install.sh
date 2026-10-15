#!/usr/bin/env bash
set -euo pipefail

# Test the installer's config patching logic.

# Syntax-check the installer script itself
bash -n installer/install.sh || { echo "FAIL: syntax error in install.sh"; exit 1; }

PASS=0
FAIL=0

# The awk program that removes the cloudhv runtime section is taken from install.sh itself, so that this test
# cannot drift from what the installer runs.
AWK_PROG="$(sed -n "/^awk '\$/,/^' \"\$CONTAINERD_CONFIG\"/p" installer/install.sh | sed '1d;$d')"
if [ -z "$AWK_PROG" ]; then
  echo "FAIL: could not extract the awk program from install.sh"; exit 1
fi

assert_contains() {
  local desc="$1" needle="$2" haystack="$3"
  if printf '%s\n' "$haystack" | grep -Fq "$needle"; then
    echo "  ✅ $desc"
    PASS=$((PASS + 1))
  else
    echo "  ❌ $desc (not found: $needle)"
    FAIL=$((FAIL + 1))
  fi
}

assert_not_contains() {
  local desc="$1" needle="$2" haystack="$3"
  if ! printf '%s\n' "$haystack" | grep -Fq "$needle"; then
    echo "  ✅ $desc"
    PASS=$((PASS + 1))
  else
    echo "  ❌ $desc (found but should not be: $needle)"
    FAIL=$((FAIL + 1))
  fi
}

# --- Test 1: awk removes existing cloudhv runtime section ---
echo ""
echo "=== Test 1: awk removes existing cloudhv runtime ==="
INPUT='[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc]
  runtime_type = "io.containerd.runc.v2"
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]
  runtime_type = "io.containerd.cloudhv.v1"
  snapshotter = "devmapper"
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kata]
  runtime_type = "io.containerd.kata.v2"'

RESULT=$(printf '%s\n' "$INPUT" | awk "$AWK_PROG")

assert_contains "runc preserved" "runtimes.runc" "$RESULT"
assert_contains "kata preserved" "runtimes.kata" "$RESULT"
assert_not_contains "cloudhv removed" "runtimes.cloudhv" "$RESULT"
assert_not_contains "devmapper removed" "devmapper" "$RESULT"

# --- Test 2: runtime config is added ---
echo ""
echo "=== Test 2: runtime config is added ==="
TOML=$(printf '%s\n' "$RESULT")
TOML="${TOML}

# Cloud Hypervisor VM-isolated runtime
[plugins.\"io.containerd.grpc.v1.cri\".containerd.runtimes.cloudhv]
  runtime_type = \"io.containerd.cloudhv.v1\""

assert_contains "cloudhv runtime" "runtimes.cloudhv" "$TOML"
assert_not_contains "no devmapper" "devmapper" "$TOML"
assert_not_contains "no erofs snapshotter on runtime" 'snapshotter = "erofs"' "$TOML"

# --- Test 3: single-quote TOML keys ---
echo ""
echo "=== Test 3: awk handles single-quote TOML keys ==="
INPUT2="[plugins.'io.containerd.grpc.v1.cri'.containerd.runtimes.cloudhv]
  runtime_type = 'io.containerd.cloudhv.v1'
[plugins.'io.containerd.grpc.v1.cri'.containerd.runtimes.runc]
  runtime_type = 'io.containerd.runc.v2'"

RESULT2=$(printf '%s\n' "$INPUT2" | awk "$AWK_PROG")

assert_not_contains "single-quote cloudhv removed" "runtimes.cloudhv" "$RESULT2"
assert_contains "single-quote runc preserved" "runtimes.runc" "$RESULT2"

# --- Test 4: running the installer's edit twice gives the same file (no growth, no duplicate sections) ---
echo ""
echo "=== Test 4: the config edit is idempotent ==="
BASE='[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc]
  runtime_type = "io.containerd.runc.v2"

[plugins."io.containerd.grpc.v1.cri".cni]
  bin_dir = "/opt/cni/bin"'
add_runtime() {
  printf '%s\n' "$1" | awk "$AWK_PROG"
  echo ""
  echo "# Cloud Hypervisor VM-isolated runtime"
  echo '[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.cloudhv]'
  echo '  runtime_type = "io.containerd.cloudhv.v1"'
}
ONCE="$(add_runtime "$BASE")"
TWICE="$(add_runtime "$ONCE")"
THRICE="$(add_runtime "$TWICE")"
if [ "$ONCE" = "$TWICE" ] && [ "$TWICE" = "$THRICE" ]; then
  echo "  ✅ same file after 1, 2 and 3 runs"
  PASS=$((PASS + 1))
else
  echo "  ❌ the file changes between runs"
  FAIL=$((FAIL + 1))
fi
assert_contains "cni section preserved" 'cni' "$TWICE"
COUNT="$(printf '%s\n' "$THRICE" | grep -c 'runtimes.cloudhv')"
if [ "$COUNT" = 1 ]; then echo "  ✅ one cloudhv section"; PASS=$((PASS + 1)); else echo "  ❌ $COUNT cloudhv sections"; FAIL=$((FAIL + 1)); fi

# --- Results ---
echo ""
echo "=== Results ==="
echo "  Passed: $PASS"
echo "  Failed: $FAIL"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
