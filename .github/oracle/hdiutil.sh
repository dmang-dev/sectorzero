#!/usr/bin/env bash
# Apple disk image cross-check, run on a macOS GitHub Actions runner (see
# .github/workflows/oracle-dmg.yml). hdiutil builds images in every format it offers, and s0 must
# read each one byte for byte like hdiutil's own raw conversion (UDTO). Where s0 can read the
# file system, the files it extracts must match the ones copied in.
#
#   .github/oracle/hdiutil.sh path/to/s0 [work dir]
#
# Exits non-zero if any check fails. Headers of the sparse images go to $W/dumps for study.
set -uo pipefail

S0=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
W=${2:-${RUNNER_TEMP:-/tmp}/dmg-oracle}
rm -rf "$W"
mkdir -p "$W/src/Folder/Sub" "$W/out" "$W/dumps" "$W/mnt"
cd "$W" || exit 1

pass=0
fail=0
ok() { echo "ok   $1"; pass=$((pass + 1)); }
bad() { echo "FAIL $1"; fail=$((fail + 1)); }

# Files to copy in: a random file large enough to span many blocks, text, a nested folder and a
# non-ASCII name (HFS+ stores names decomposed).
head -c 4194304 /dev/urandom > src/random.bin
printf 'SectorZero hdiutil oracle\n' > src/readme.txt
seq 1 20000 > src/Folder/numbers.txt
printf 'deep\n' > src/Folder/Sub/deep.txt
printf 'caf\xc3\xa9\n' > "src/caf$(printf '\xc3\xa9').txt"

sha_of_s0() { "$S0" checksum --json "$1" | python3 -c 'import json,sys; print(json.load(sys.stdin)["sha256"])'; }
sha_of_file() { shasum -a 256 "$1" | cut -d' ' -f1; }

# Make a writable image with the given layout and file system, and copy the files in.
make_rw() { # name layout fs size
  hdiutil create -quiet -size "$4" -layout "$2" -fs "$3" -volname "SZ Oracle" -type UDIF "$1.dmg" ||
    return 1
  hdiutil attach -quiet -nobrowse -noverify -mountpoint "$W/mnt" "$1.dmg" || return 1
  cp -R src/. "$W/mnt/"
  hdiutil detach -quiet "$W/mnt" || hdiutil detach -force "$W/mnt"
}

# hdiutil's raw view of an image, as the reference.
reference() { # image name
  rm -f "ref-$2.cdr"
  hdiutil convert -quiet "$1" -format UDTO -o "ref-$2" >/dev/null || return 1
  sha_of_file "ref-$2.cdr"
}

# The whole check for one image: s0 opens it, the disk hashes like hdiutil's raw conversion, and
# (when files is set) the extracted files match src.
check_image() { # image name files(yes/no)
  local img=$1 name=$2 files=$3 want got
  if ! "$S0" info "$img" > "out/$name.info" 2>&1; then
    bad "$name: s0 info: $(tail -1 "out/$name.info")"
    return
  fi
  want=$(reference "$img" "$name") || { bad "$name: hdiutil convert failed"; return; }
  got=$(sha_of_s0 "$img" 2> "out/$name.err") || { bad "$name: s0 checksum: $(tail -1 "out/$name.err")"; return; }
  if [ "$want" = "$got" ]; then ok "$name: disk matches hdiutil ($got)"; else bad "$name: disk $got, hdiutil $want"; fi
  rm -f "ref-$name.cdr"
  if [ "$files" = yes ]; then
    rm -rf "out/$name"
    if ! "$S0" extract "$img" -o "out/$name" > "out/$name.extract" 2>&1; then
      bad "$name: s0 extract: $(tail -1 "out/$name.extract")"
    elif diff -r -x '.fseventsd' -x '.Trashes' -x '.Spotlight-V100' -x '.DS_Store' \
        -x '.HFS+ Private Directory Data*' -x '*HFS+ Private Data' -x '.journal*' \
        src "out/$name" > "out/$name.diff" 2>&1; then
      ok "$name: extracted files match"
    else
      bad "$name: extracted files differ: $(head -3 "out/$name.diff" | tr '\n' ' ')"
    fi
    rm -rf "out/$name"
  fi
}

sw_vers
hdiutil help 2>&1 | head -1 || true

# 1. One HFS+ disk (GPT, the hdiutil default) in every UDIF and sparse format.
make_rw base GPTSPUD HFS+J 32m || { bad "could not build the base image"; exit 1; }
check_image base.dmg UDRW yes
for fmt in UDRO UDCO UDZO UDBZ ULFO ULMO; do
  hdiutil convert -quiet base.dmg -format "$fmt" -o "$fmt.dmg" >/dev/null ||
    { bad "$fmt: hdiutil convert failed"; continue; }
  check_image "$fmt.dmg" "$fmt" yes
done
hdiutil convert -quiet base.dmg -format UDZO -imagekey zlib-level=9 -o UDZO9.dmg >/dev/null &&
  check_image UDZO9.dmg UDZO-level9 yes
hdiutil convert -quiet base.dmg -format UDSP -o sparse >/dev/null && check_image sparse.sparseimage UDSP yes
hdiutil convert -quiet base.dmg -format UDSB -o bundle >/dev/null && check_image bundle.sparsebundle UDSB yes

# 2. Split images: UDRO and UDZO cut into 1 MiB segments.
for fmt in UDRO UDZO; do
  rm -f seg-"$fmt"*
  if hdiutil segment -quiet -segmentSize 1m -o "seg-$fmt" "$fmt.dmg"; then
    ls seg-"$fmt"* | sed 's/^/     segment /'
    check_image "seg-$fmt.dmg" "segmented-$fmt" yes
  else
    bad "segmented-$fmt: hdiutil segment failed"
  fi
done

# 3. Other partition layouts and file systems.
make_rw apm SPUD HFS+J 16m && hdiutil convert -quiet apm.dmg -format UDZO -o apm-z.dmg >/dev/null &&
  check_image apm-z.dmg "APM-HFS+" yes
make_rw mbr MBRSPUD HFS+J 16m && hdiutil convert -quiet mbr.dmg -format UDZO -o mbr-z.dmg >/dev/null &&
  check_image mbr-z.dmg "MBR-HFS+" yes
make_rw bare NONE HFS+ 16m && hdiutil convert -quiet bare.dmg -format UDZO -o bare-z.dmg >/dev/null &&
  check_image bare-z.dmg "bare-HFS+" yes
make_rw fat MBRSPUD "MS-DOS FAT32" 40m && hdiutil convert -quiet fat.dmg -format ULFO -o fat-z.dmg >/dev/null &&
  check_image fat-z.dmg "MBR-FAT32" yes
make_rw exfat GPTSPUD ExFAT 16m && hdiutil convert -quiet exfat.dmg -format UDBZ -o exfat-z.dmg >/dev/null &&
  check_image exfat-z.dmg "GPT-exFAT" yes
# APFS: s0 cannot read APFS volumes, so only the disk is compared.
make_rw apfs GPTSPUD APFS 32m && hdiutil convert -quiet apfs.dmg -format ULFO -o apfs-z.dmg >/dev/null &&
  check_image apfs-z.dmg "GPT-APFS" no

# 4. A disk image made straight from a folder (hdiutil picks everything itself; on current
# macOS that is an APFS volume, so only the disk is compared).
hdiutil create -quiet -srcfolder src -volname "From Folder" -o folder.dmg >/dev/null &&
  check_image folder.dmg "srcfolder-default" no

# 5. Encrypted images must be refused with the encrypted-image error.
for kind in UDIF SPARSE SPARSEBUNDLE; do
  rm -rf enc-"$kind".*
  printf 'oracle-test-password' | hdiutil create -quiet -encryption AES-128 -stdinpass -size 8m \
    -fs HFS+ -type "$kind" -o "enc-$kind" >/dev/null || { bad "encrypted $kind: create failed"; continue; }
  img=$(ls -d enc-"$kind".* | head -1)
  if "$S0" info "$img" > "out/enc-$kind.err" 2>&1; then
    bad "encrypted $kind: s0 opened it"
  elif grep -qi encrypted "out/enc-$kind.err"; then
    ok "encrypted $kind ($img) refused: $(tail -1 "out/enc-$kind.err")"
  else
    bad "encrypted $kind: wrong error: $(tail -1 "out/enc-$kind.err")"
  fi
done

# 6. Sparse images with more than 1008 bands in use. Each header maps 1008 bands; hdiutil then
# writes another header after the last band and links it from offset 20. This one passes 4 GiB,
# so the link is shown to be 64 bits wide. The dumps keep every header for study.
cat > dump_sparse.py <<'EOF'
import struct, sys
path, out = sys.argv[1], sys.argv[2]
with open(path, "rb") as f, open(out + ".txt", "w") as t:
    f.seek(0, 2)
    print("file size", f.tell(), file=t)
    at, k = 0, 0
    while True:
        f.seek(at)
        h = f.read(4096)
        with open(f"{out}.head{k}", "wb") as o:
            o.write(h)
        words = struct.unpack(">4s15I", h[:64])
        used = [v for v in struct.unpack(">1008I", h[64:4096]) if v]
        print(f"header {k} at {at}:", [hex(x) if isinstance(x, int) else x for x in words], file=t)
        print(f"  entries used {len(used)}, bands {min(used, default=0)}..{max(used, default=0)}", file=t)
        nxt = struct.unpack(">Q", h[20:28])[0]
        if not nxt or nxt <= at:
            break
        at, k = nxt, k + 1
EOF
rm -f big.sparseimage
if hdiutil create -quiet -size 4500m -type SPARSE -layout NONE -fs HFS+ -volname Big big >/dev/null &&
  hdiutil attach -quiet -nobrowse -noverify -mountpoint "$W/mnt" big.sparseimage; then
  dd if=/dev/urandom of="$W/mnt/fill.bin" bs=1m count=4300 2>/dev/null
  hdiutil detach -quiet "$W/mnt" || hdiutil detach -force "$W/mnt"
  python3 dump_sparse.py big.sparseimage "$W/dumps/big" && cat "$W/dumps/big.txt"
  check_image big.sparseimage "sparse-over-4GiB-chained-headers" no
  rm -f big.sparseimage
else
  bad "could not build the large sparse image"
fi
python3 dump_sparse.py sparse.sparseimage "$W/dumps/small" && cat "$W/dumps/small.txt"

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
