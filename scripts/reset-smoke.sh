#!/usr/bin/env bash
# for every resettable store: dirty the served image, reset, prove the bytes
# came back. fast, correctness only, no timing claims.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/blkrevertd"
CTL="$ROOT/scripts/ctl.py"
W="$ROOT/work/smoke"
GOLD="$W/golden.img"
SIZE_MIB="${SIZE_MIB:-64}"

pass=0; fail=0
ck() { if [ "$2" = "$3" ]; then printf '    ok   %-42s %s\n' "$1" "$2"; pass=$((pass+1));
       else printf '    FAIL %-42s got %s want %s\n' "$1" "$2" "$3"; fail=$((fail+1)); fi; }

cleanup() { [ -n "${DPID:-}" ] && kill -TERM "$DPID" 2>/dev/null
            sleep 0.3; fusermount3 -u -z "$W/mnt" 2>/dev/null
            [ -n "${DPID:-}" ] && kill -KILL "$DPID" 2>/dev/null; return 0; }
trap cleanup EXIT

rm -rf "$W"; mkdir -p "$W/mnt"
dd if=/dev/urandom of="$GOLD" bs=1M count="$SIZE_MIB" status=none
GSUM=$(sha256sum "$GOLD" | cut -d' ' -f1)

for store in fullcopy dirtylog overlay cow; do
  echo "  -- $store --"
  args=(--golden "$GOLD" --store "$store" --mountpoint "$W/mnt" --name rootfs.img
        --control "$W/ctl.sock" --threads 4 --log-level warn)
  case $store in
    fullcopy|dirtylog) args+=(--image "$W/private.img" --reseed) ;;
    overlay)           args+=(--image "$W/overlay.img") ;;
  esac

  "$BIN" "${args[@]}" >"$W/d-$store.log" 2>&1 &
  DPID=$!
  for _ in $(seq 1 60); do [ -S "$W/ctl.sock" ] && mountpoint -q "$W/mnt" && break; sleep 0.1; done
  if ! mountpoint -q "$W/mnt"; then echo "    FAIL did not mount"; cat "$W/d-$store.log"; fail=$((fail+1)); continue; fi

  SERVED="$W/mnt/rootfs.img"
  ck "serves the golden bytes" "$(sha256sum "$SERVED" | cut -d' ' -f1)" "$GSUM"
  ck "reports resettable"      "$("$CTL" "$W/ctl.sock" info | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["store"]["resettable"])')" "True"

  # dirty three separate regions so the run coalescing is exercised. the
  # offsets scale with the image, because a write past the end would extend the
  # file and the comparison against the golden would then fail for the wrong
  # reason.
  BLOCKS=$(( SIZE_MIB * 1024 / 4 ))
  for frac in 10 40 80; do
    seek=$(( BLOCKS * frac / 100 ))
    dd if=/dev/urandom of="$SERVED" bs=4096 seek=$seek count=8 conv=notrunc status=none 2>/dev/null
  done
  # and one deliberately unaligned write, to exercise read-modify-write
  printf 'unaligned-%s' "$store" | dd of="$SERVED" bs=1 seek=65537 conv=notrunc status=none 2>/dev/null
  sync
  ck "dirtied bytes are visible"  "$([ "$(sha256sum "$SERVED" | cut -d' ' -f1)" != "$GSUM" ] && echo changed || echo same)" "changed"
  ck "dirty blocks are tracked"   "$("$CTL" "$W/ctl.sock" info | python3 -c 'import json,sys;d=json.load(sys.stdin)["result"]["store"];print(d["dirty_blocks"]>0 or d["kind"]=="fullcopy")')" "True"

  R=$("$CTL" "$W/ctl.sock" reset)
  ck "reset reports ok"           "$(echo "$R" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ok"])')" "True"
  ck "reset invalidated the cache" "$(echo "$R" | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["invalidated"])')" "True"
  ck "image is byte identical again" "$(sha256sum "$SERVED" | cut -d' ' -f1)" "$GSUM"
  ck "nothing private is left"    "$("$CTL" "$W/ctl.sock" info | python3 -c 'import json,sys;d=json.load(sys.stdin)["result"]["store"];print(d["dirty_blocks"])')" "0"

  # mark: dirty again, move the reference, then reset must keep the new bytes
  dd if=/dev/urandom of="$SERVED" bs=4096 seek=$(( BLOCKS / 20 )) count=4 conv=notrunc status=none 2>/dev/null
  sync
  MSUM=$(sha256sum "$SERVED" | cut -d' ' -f1)
  M=$("$CTL" "$W/ctl.sock" mark)
  ck "mark reports ok"            "$(echo "$M" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ok"])')" "True"
  "$CTL" "$W/ctl.sock" reset >/dev/null
  ck "reset now returns to the marked state" "$(sha256sum "$SERVED" | cut -d' ' -f1)" "$MSUM"

  "$CTL" "$W/ctl.sock" shutdown >/dev/null 2>&1
  wait $DPID 2>/dev/null
  DPID=""
  # restore the golden for the next store, since mark rewrote it
  dd if=/dev/urandom of="$GOLD" bs=1M count="$SIZE_MIB" status=none
  GSUM=$(sha256sum "$GOLD" | cut -d' ' -f1)
done

echo
echo "  passed $pass, failed $fail"
[ "$fail" -eq 0 ]
