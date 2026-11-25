#!/usr/bin/env bash
# EnCase (EWF) cross-check, run on an Ubuntu GitHub Actions runner (see
# .github/workflows/oracle-ewf.yml). ewfacquire from libewf's ewf-tools builds evidence files in
# every format it writes, from disks s0 made, and s0 must read each one byte for byte like the
# source. Where s0 can read the file system, the files it extracts must match the ones copied
# in. libewf is only run here as a black-box tool.
#
#   .github/oracle/ewfacquire.sh path/to/s0 [work dir]
#
# Exits non-zero if any check fails. Small sample files go to $W/samples.
set -uo pipefail

S0=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
W=${2:-${RUNNER_TEMP:-/tmp}/ewf-oracle}
rm -rf "$W"
mkdir -p "$W/src/Folder/Sub" "$W/e" "$W/out" "$W/samples"
cd "$W" || exit 1

pass=0
fail=0
ok() { echo "ok   $1"; pass=$((pass + 1)); }
bad() { echo "FAIL $1"; fail=$((fail + 1)); }

ewfacquire -V 2>&1 | head -1

head -c 3000000 /dev/urandom > src/random.bin
printf 'SectorZero ewfacquire oracle\n' > src/readme.txt
seq 1 20000 > src/Folder/numbers.txt
printf 'deep\n' > src/Folder/Sub/deep.txt

# Source disks: an MBR disk with a FAT volume holding the files; the same with 7 extra sectors
# (not a whole number of chunks); 150 MiB of random data (for more than 99 segments); and a
# small disk for the sample files.
"$S0" new disk.img --size 64M --table mbr -l ORACLE > /dev/null || { bad "s0 new failed"; exit 1; }
"$S0" inject disk.img src/random.bin src/readme.txt src/Folder > /dev/null || { bad "s0 inject failed"; exit 1; }
cp disk.img odd.img
head -c 3584 /dev/urandom >> odd.img
head -c 157286400 /dev/urandom > big.img
"$S0" new small.img --size 4M --table mbr -l SAMPLE > /dev/null
"$S0" inject small.img src/readme.txt src/Folder > /dev/null
mkdir -p small-src/Folder/Sub
cp src/readme.txt small-src/ && cp src/Folder/numbers.txt small-src/Folder/ && cp src/Folder/Sub/deep.txt small-src/Folder/Sub/

# ewfacquire SOURCE NAME [options]: writes e/NAME.E01 (…), case details filled in.
acquire() {
  local source=$1 name=$2
  shift 2
  rm -f "e/$name".*
  ewfacquire -u -q -t "e/$name" -C "Case 42" -D "Oracle image" -E "Evidence 1" -e "Examiner" \
    -N "Notes" "$@" "$source" < /dev/null > "out/$name.acquire" 2>&1
}

first_segment() { ls e/"$1".* 2> /dev/null | grep -Ei '\.(E01|s01|Ex01)$' | head -1; }

# The whole check for one evidence file: s0 opens it, the disk hashes like the source, and
# (when files is set) the extracted files match.
check() { # name source files(yes/no) [files dir]
  local name=$1 source=$2 files=$3 dir=${4:-src} img want got
  img=$(first_segment "$name")
  if [ -z "$img" ]; then
    bad "$name: ewfacquire made no file: $(tail -2 "out/$name.acquire" | tr '\n' ' ')"
    return
  fi
  if ! "$S0" info "$img" > "out/$name.info" 2>&1; then
    bad "$name: s0 info: $(tail -1 "out/$name.info")"
    return
  fi
  want=$(sha256sum "$source" | cut -d' ' -f1)
  got=$("$S0" checksum --json "$img" 2> "out/$name.err" | python3 -c 'import json,sys; print(json.load(sys.stdin)["sha256"])') ||
    { bad "$name: s0 checksum: $(tail -1 "out/$name.err")"; return; }
  local segs
  segs=$(ls e/"$name".* | wc -l)
  if [ "$want" = "$got" ]; then ok "$name: disk matches ($segs segment files)"; else bad "$name: disk $got, source $want"; fi
  if [ "$files" = yes ]; then
    rm -rf "out/$name"
    if ! "$S0" extract "$img" -o "out/$name" > "out/$name.extract" 2>&1; then
      bad "$name: s0 extract: $(tail -1 "out/$name.extract")"
    elif diff -r "$dir" "out/$name" > "out/$name.diff" 2>&1; then
      ok "$name: extracted files match"
    else
      bad "$name: extracted files differ: $(head -3 "out/$name.diff" | tr '\n' ' ')"
    fi
    rm -rf "out/$name"
  fi
}

# 1. Every format ewfacquire writes, best compression.
for f in ewf smart ftk encase1 encase2 encase3 encase4 encase5 encase6 encase7 linen5 linen6 linen7 ewfx; do
  acquire disk.img "$f" -f "$f" -c best
  check "$f" disk.img yes
done

# 2. Compression levels, chunk sizes, 4096-byte sectors and an odd size (EnCase 6).
for c in none empty-block fast; do
  acquire disk.img "encase6-$c" -f encase6 -c "$c"
  check "encase6-$c" disk.img yes
done
for b in 16 1024 32768; do
  acquire disk.img "encase6-chunk$b" -f encase6 -c best -b "$b"
  check "encase6-chunk$b" disk.img yes
done
acquire disk.img encase6-4k -f encase6 -c best -P 4096
check encase6-4k disk.img no
acquire odd.img encase6-odd -f encase6 -c best
check encase6-odd odd.img no

# 3. Segments: 1 MiB segment files, compressed and stored, SMART too, and more than 99 (.EAA …).
acquire disk.img seg-best -f encase6 -c best -S 1MiB
check seg-best disk.img yes
acquire disk.img seg-smart -f smart -c none -S 1MiB
check seg-smart disk.img yes
acquire big.img seg-many -f encase6 -c none -S 1MiB
ls e/seg-many.* | sed 's/.*\.//' | sort | tr '\n' ' ' | head -c 600
echo
check seg-many big.img no

# 4. Damage and mistakes must give errors, not wrong data.
expect_error() { # label pattern image
  if "$S0" checksum "$3" > "out/err.txt" 2>&1; then
    bad "$1: s0 read it"
  elif grep -qi "$2" out/err.txt; then
    ok "$1: $(tail -1 out/err.txt)"
  else
    bad "$1: unexpected error: $(tail -1 out/err.txt)"
  fi
}
for name in encase6-none encase6; do
  cp "e/$name.E01" "e/$name-damaged.E01"
  size=$(stat -c %s "e/$name.E01")
  printf '\x55\x55\x55\x55\x55\x55\x55\x55\x55\x55\x55\x55\x55\x55\x55\x55' |
    dd of="e/$name-damaged.E01" bs=1 seek=$((size / 2)) conv=notrunc 2> /dev/null
  expect_error "$name damaged chunk" "damaged" "e/$name-damaged.E01"
done
expect_error "later segment opened" "later segment" e/seg-best.E02
mv e/seg-best.E03 e/seg-best.E03.away
expect_error "missing segment" "seg-best.E03" e/seg-best.E01
mv e/seg-best.E03.away e/seg-best.E03

# 5. EnCase 7 Ex01 (EWF version 2), if this ewfacquire writes it. s0 does not read it yet.
acquire small.img v2 -f encase7-v2 -c best
if [ -n "$(first_segment v2)" ]; then
  echo "note encase7-v2: $("$S0" info "$(first_segment v2)" 2>&1 | tail -1)"
else
  echo "note encase7-v2: ewfacquire made no file: $(tail -1 out/v2.acquire)"
fi

# 6. Small samples of each format for study and for the test suite.
for f in encase1 encase5 encase6 encase7 ftk smart linen6 ewfx; do
  acquire small.img "sample-$f" -f "$f" -c best
  check "sample-$f" small.img yes small-src
done
acquire small.img sample-encase6-none -f encase6 -c none -S 1MiB
check sample-encase6-none small.img yes small-src
cp e/sample-* e/v2.* samples/ 2> /dev/null
cp small.img samples/
ewfinfo e/sample-encase6.E01 > samples/ewfinfo-encase6.txt 2>&1 || true
ls -l samples

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
