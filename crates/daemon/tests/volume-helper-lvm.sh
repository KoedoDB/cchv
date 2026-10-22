#!/bin/sh
# Tests the volume helper (crates/daemon/src/lvm.rs) against the real LVM tools, on a volume group made on a loop device.
#
# Run it in a throw-away privileged container with hard limits (it makes loop and device-mapper devices, which the kernel shares with the host),
# on a build machine, never on a node:
#
#   docker run --rm --privileged -m 8g --memory-swap 8g --cpus 4 -v <repo>:/src alpine:3.24 sh /src/crates/daemon/tests/volume-helper-lvm.sh
#
# The volume group has a random name, lives in a 512 MiB sparse file, and is removed at the end (and on an error). It needs no udev: the
# configuration below makes LVM make its own nodes.
set -eu

apk add --no-cache lld protoc cargo rust make musl-dev git lvm2 util-linux-misc e2fsprogs python3 coreutils diffutils losetup >/dev/null

mkdir -p /etc/lvm
cat > /etc/lvm/lvm.conf <<'EOF'
activation { udev_sync = 0
             udev_rules = 0 }
devices { global_filter = [ "a|^/dev/loop[0-9]+$|", "r|.*|" ] }
EOF

WORK=/tmp/vh
rm -rf "$WORK"; mkdir -p "$WORK"
cp -r /src "$WORK/src"
cd "$WORK/src"
export CARGO_TARGET_DIR="$WORK/target"
cargo build --locked -p cloudhv-sandbox-daemon 2>&1 | tail -2
BIN="$WORK/target/debug/cloudhv-sandbox-daemon"

VG="vgtest$$"
IMG="$WORK/pv.img"
truncate -s 512M "$IMG"
# The container's /dev has no loop nodes: make the first few, and the control node.
[ -c /dev/loop-control ] || mknod /dev/loop-control c 10 237
for n in 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do [ -b /dev/loop$n ] || mknod /dev/loop$n b 7 $n; done
LOOP="$(losetup -f --show "$IMG")"
HELPER_PID=""
cleanup() {
  [ -n "$HELPER_PID" ] && kill "$HELPER_PID" 2>/dev/null || true
  for lv in $(lvs --noheadings -o lv_name "$VG" 2>/dev/null); do lvremove -f "$VG/$lv" >/dev/null 2>&1 || true; done
  vgremove -f "$VG" >/dev/null 2>&1 || true
  pvremove -ff -y "$LOOP" >/dev/null 2>&1 || true
  losetup -d "$LOOP" 2>/dev/null || true
}
trap cleanup EXIT
pvcreate -ff -y "$LOOP" >/dev/null
vgcreate "$VG" "$LOOP" >/dev/null

SOCK="$WORK/helper.sock"
cat > "$WORK/d.json" <<EOF
{"kernel_path":"/k","rootfs_path":"/r","volume_block_vg":"$VG","volume_block_max_mib":200,"volume_helper_socket":"$SOCK","lvm_bin_dir":"/sbin"}
EOF
"$BIN" --volume-helper "$WORK/d.json" > "$WORK/helper.log" 2>&1 &
HELPER_PID=$!
for i in 1 2 3 4 5 6 7 8 9 10; do [ -S "$SOCK" ] && break; sleep 0.5; done
[ -S "$SOCK" ] || { cat "$WORK/helper.log"; echo "the helper did not start"; exit 1; }
[ "$(stat -c %a "$SOCK")" = 600 ] || { echo "the socket is not 0600"; exit 1; }

cat > "$WORK/call.py" <<'EOF'
import json, socket, sys
req = json.loads(sys.argv[1])
s = socket.socket(socket.AF_UNIX); s.settimeout(300); s.connect(sys.argv[2])
s.sendall((json.dumps(req) + "\n").encode())
data = b""
while not data.endswith(b"\n"):
    c = s.recv(65536)
    if not c: break
    data += c
print(data.decode().strip())
EOF
call() { python3 "$WORK/call.py" "$1" "$SOCK"; }
fail() { echo "FAIL: $*"; cat "$WORK/helper.log" | tail -20; exit 1; }
# The first 6 bytes of a volume (O_DIRECT reads are whole sectors).
marker() { dd if="/dev/$VG/$1" bs=512 count=1 iflag=direct 2>/dev/null | head -c 6; }
field() { python3 -c 'import json,sys; d=json.loads(sys.argv[1]); print(eval(sys.argv[2]))' "$1" "$2"; }

echo "== stale data: a volume with 0xFF is removed, the next volume gets its extents"
lvcreate -y -Zn -L 64m -n stale "$VG" >/dev/null
tr '\0' '\377' < /dev/zero | dd of="/dev/$VG/stale" bs=1M count=64 oflag=direct 2>/dev/null
lvremove -f "$VG/stale" >/dev/null

echo "== ensure: a new volume is made, zeroed whole, tagged and ready"
R="$(call '{"op":"ensure","namespace":"p-abc-db","volume":"data","size_mib":64}')"
[ "$(field "$R" "d['ok']")" = True ] || fail "ensure: $R"
[ "$(field "$R" "d['created']")" = True ] || fail "not created: $R"
LV=pv-p-abc-db_data
TAGS="$(lvs --noheadings -o lv_tags "$VG/$LV" | tr -d ' ')"
echo "tags: $TAGS"
case "$TAGS" in *cloudhv-ns=p-abc-db*) ;; *) fail "no namespace tag: $TAGS";; esac
case "$TAGS" in *cloudhv-ready*) ;; *) fail "not ready: $TAGS";; esac
cmp -n $((64*1024*1024)) "/dev/$VG/$LV" /dev/zero || fail "the new volume is not zero"
[ "$(field "$R" "d['info']['size_mib']")" = 64 ] || fail "size"
MAJ="$(field "$R" "d['info']['major']")"; MIN="$(field "$R" "d['info']['minor']")"
[ "$(cat /sys/dev/block/$MAJ:$MIN/dm/name)" = "$VG-pv--p--abc--db_data" ] || fail "dm name: $(cat /sys/dev/block/$MAJ:$MIN/dm/name)"

echo "== ensure again: nothing is zeroed or changed (the data stays)"
printf 'MARKER' | dd of="/dev/$VG/$LV" bs=512 count=1 oflag=direct conv=sync 2>/dev/null
R="$(call '{"op":"ensure","namespace":"p-abc-db","volume":"data","size_mib":64}')"
[ "$(field "$R" "d['created']")" = False ] || fail "created again"
[ "$(marker "$LV")" = MARKER ] || fail "the data is gone"

echo "== a smaller size does not shrink"
R="$(call '{"op":"ensure","namespace":"p-abc-db","volume":"data","size_mib":8}')"
[ "$(field "$R" "d['info']['size_mib']")" = 64 ] || fail "shrunk: $R"

echo "== the cap"
R="$(call '{"op":"ensure","namespace":"p-abc-db","volume":"big","size_mib":201}')"
[ "$(field "$R" "d['ok']")" = False ] || fail "over the cap was made: $R"
lvs "$VG/pv-p-abc-db_big" >/dev/null 2>&1 && fail "the over-cap volume exists"

echo "== extend: the new part is zeroed, the data stays"
lvcreate -y -Zn -L 64m -n stale2 "$VG" >/dev/null
tr '\0' '\377' < /dev/zero | dd of="/dev/$VG/stale2" bs=1M count=64 oflag=direct 2>/dev/null
lvremove -f "$VG/stale2" >/dev/null
R="$(call '{"op":"ensure","namespace":"p-abc-db","volume":"data","size_mib":128}')"
[ "$(field "$R" "d['extended']")" = True ] || fail "not extended: $R"
[ "$(field "$R" "d['info']['size_mib']")" = 128 ] || fail "size after extend: $R"
[ "$(marker "$LV")" = MARKER ] || fail "the data is gone after the extension"
cmp -i $((64*1024*1024)) -n $((64*1024*1024)) "/dev/$VG/$LV" /dev/zero || fail "the added part is not zero"
case "$(lvs --noheadings -o lv_tags "$VG/$LV")" in *cloudhv-extending*) fail "extending tag left";; esac

echo "== an extension that did not end: the tag finishes the zeroing of the new part, never the data"
lvcreate -y -Zn -L 64m -n stale3 "$VG" >/dev/null
tr '\0' '\377' < /dev/zero | dd of="/dev/$VG/stale3" bs=1M count=64 oflag=direct 2>/dev/null
lvremove -f "$VG/stale3" >/dev/null
lvchange --addtag cloudhv-extending=128 "$VG/$LV" >/dev/null
lvextend -y -L 192m "$VG/$LV" >/dev/null
R="$(call '{"op":"ensure","namespace":"p-abc-db","volume":"data","size_mib":192}')"
[ "$(field "$R" "d['ok']")" = True ] || fail "recovery: $R"
[ "$(marker "$LV")" = MARKER ] || fail "the data is gone after the recovery"
cmp -i $((128*1024*1024)) -n $((64*1024*1024)) "/dev/$VG/$LV" /dev/zero || fail "the part of the unfinished extension is not zero"

echo "== a creation that did not end (owner tag, no ready tag) is zeroed before it is handed out"
lvcreate -y -Zn -L 32m -n stale4 "$VG" >/dev/null
tr '\0' '\377' < /dev/zero | dd of="/dev/$VG/stale4" bs=1M count=32 oflag=direct 2>/dev/null
lvremove -f "$VG/stale4" >/dev/null
lvcreate -y -Zn -L 32m --addtag cloudhv-ns=p-half-db -n pv-p-half-db_data "$VG" >/dev/null
R="$(call '{"op":"ensure","namespace":"p-half-db","volume":"data","size_mib":32}')"
[ "$(field "$R" "d['ok']")" = True ] || fail "half: $R"
cmp -n $((32*1024*1024)) "/dev/$VG/pv-p-half-db_data" /dev/zero || fail "the unfinished volume was handed out unzeroed"

echo "== a volume of another namespace, and one without an owner, are refused and untouched"
lvchange --deltag cloudhv-ns=p-abc-db --addtag cloudhv-ns=p-zzz-db "$VG/$LV" >/dev/null
R="$(call '{"op":"ensure","namespace":"p-abc-db","volume":"data","size_mib":192}')"
[ "$(field "$R" "d['ok']")" = False ] || fail "another namespace's volume was given: $R"
lvchange --deltag cloudhv-ns=p-zzz-db "$VG/$LV" >/dev/null
R="$(call '{"op":"ensure","namespace":"p-abc-db","volume":"data","size_mib":192}')"
[ "$(field "$R" "d['ok']")" = False ] || fail "a volume without an owner was adopted: $R"
[ "$(marker "$LV")" = MARKER ] || fail "a refused volume was touched"
lvchange --addtag cloudhv-ns=p-abc-db "$VG/$LV" >/dev/null

echo "== names the helper does not accept"
for bad in '{"op":"ensure","namespace":"P","volume":"data","size_mib":8}' '{"op":"ensure","namespace":"p-a-db","volume":"../x","size_mib":8}' '{"op":"lvremove","name":"x"}' 'not json'; do
  R="$(python3 "$WORK/call.py" "$(printf '%s' "$bad" | python3 -c 'import sys,json; s=sys.stdin.read()
try: json.loads(s); print(s)
except Exception: print(json.dumps({"op":"junk"}))')" "$SOCK")"
  [ "$(field "$R" "d['ok']")" = False ] || fail "accepted: $bad -> $R"
done

echo "== tags: by device numbers, and a device of no volume group of ours"
R="$(call "{\"op\":\"tags\",\"major\":$MAJ,\"minor\":$MIN}")"
[ "$(field "$R" "d['info']['lv']")" = "$LV" ] || fail "tags: $R"
R="$(call '{"op":"tags","major":7,"minor":0}')"
[ "$(field "$R" "d.get('info')")" = None ] || fail "tags of a loop device: $R"

echo "== the volume helper survives all of it"
kill -0 "$HELPER_PID" || fail "the helper died"
echo "ALL OK"
