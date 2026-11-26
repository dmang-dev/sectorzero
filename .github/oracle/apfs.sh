#!/usr/bin/env bash
# APFS cross-check, run on a macOS GitHub Actions runner (see .github/workflows/oracle-apfs.yml).
# macOS builds APFS volumes in disk images and fills them with files (apfs_tree.py populate);
# s0 must extract every file, folder and link exactly as macOS shows them when the image is
# mounted read-only. Containers with several volumes and an encrypted volume are covered too.
#
#   .github/oracle/apfs.sh path/to/s0 [work dir]
#
# Exits non-zero if any check fails. Small sample images with manifests go to $W/samples.
set -uo pipefail

S0=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
TREE=$(cd "$(dirname "$0")" && pwd)/apfs_tree.py
W=${2:-${RUNNER_TEMP:-/tmp}/apfs-oracle}
rm -rf "$W"
mkdir -p "$W/out" "$W/samples" "$W/root"
cd "$W" || exit 1

pass=0
fail=0
ok() { echo "ok   $1"; pass=$((pass + 1)); }
bad() { echo "FAIL $1"; fail=$((fail + 1)); }

sw_vers
command -v afsctool > /dev/null || brew install --quiet afsctool > /dev/null 2>&1 ||
  echo "note: afsctool could not be installed; compressed files come from ditto only"

# Attach an image with its volumes under $W/root/<volume name>; print the whole-disk device.
attach() { # image [-readonly]
  hdiutil attach -nobrowse -noverify -owners on -mountroot "$W/root" ${2:-} "$1" | head -1 | cut -f1 | tr -d ' '
}
detach() { hdiutil detach -quiet "$1" || hdiutil detach -quiet -force "$1"; }

# A new image with one APFS volume named "$1", filled by apfs_tree.py, then compressed (UDZO,
# or the format in $FORMAT). $EXTRA runs before the volume is detached.
build() { # name size fs [--small|--tiny]
  hdiutil create -quiet -size "$2" -fs "$3" -volname "$1" -layout GPTSPUD -type UDIF "$1-rw.dmg" ||
    return 1
  local dev
  dev=$(attach "$1-rw.dmg") || return 1
  python3 "$TREE" populate "$W/root/$1" ${4:-} || { detach "$dev"; return 1; }
  if [ -n "${EXTRA:-}" ]; then eval "$EXTRA"; fi
  python3 "$TREE" decmpfs "$W/root/$1"
  detach "$dev"
  hdiutil convert -quiet "$1-rw.dmg" -format "${FORMAT:-UDZO}" -o "$1.dmg" > /dev/null && rm -f "$1-rw.dmg"
}

# s0 extracts volume $3 (default the first) of image $1, data only and with AppleDouble files,
# and both must match the read-only mount at $W/root/$2. The files go under $XDIR (default
# out/; a case-sensitive volume for case-sensitive images).
check() { # image volume-name [--volume N]
  local img=$1 vol=$2 name x=${XDIR:-out}
  shift 2
  name=$(echo "$img-$vol" | tr -c 'A-Za-z0-9.\n-' '_')
  if ! "$S0" info "$img" "$@" > "out/$name.info" 2>&1; then
    bad "$name: s0 info: $(tail -1 "out/$name.info")"
    return
  fi
  local how
  for how in data appledouble; do
    rm -rf "$x/$name-$how"
    if ! "$S0" extract "$img" "$@" --forks "$how" -o "$x/$name-$how" > "out/$name-$how.extract" 2>&1; then
      bad "$name ($how): s0 extract: $(tail -1 "out/$name-$how.extract")"
      continue
    fi
    local flag=
    [ "$how" = appledouble ] && flag=--forks
    if python3 "$TREE" compare "$W/root/$vol" "$x/$name-$how" $flag > "out/$name-$how.diff" 2>&1; then
      ok "$name ($how): $(tail -1 "out/$name-$how.diff")"
    else
      bad "$name ($how): $(head -4 "out/$name-$how.diff" | tr '\n' ' ')"
    fi
    rm -rf "$x/$name-$how"
  done
}

expect_error() { # label pattern image [args]
  local label=$1 pattern=$2
  shift 2
  if "$S0" ls "$@" > out/err.txt 2>&1; then
    bad "$label: s0 read it"
  elif grep -qi "$pattern" out/err.txt; then
    ok "$label: $(tail -1 out/err.txt)"
  else
    bad "$label: unexpected error: $(tail -1 out/err.txt)"
  fi
}

# 1. A large, full volume (case-insensitive APFS, the default).
if build "SZ APFS" 400m APFS; then
  dev=$(attach "SZ APFS.dmg" -readonly)
  check "SZ APFS.dmg" "SZ APFS"
  detach "$dev"
else
  bad "could not build the APFS image"
fi

# 2. Case-sensitive APFS, with two names that differ only in case.
EXTRA='printf "upper\n" > "$W/root/SZ Case/Readme.txt"' FORMAT=ULMO \
  build "SZ Case" 24m "Case-sensitive APFS" --tiny || bad "could not build the case-sensitive image"
dev=$(attach "SZ Case.dmg" -readonly)
hdiutil create -quiet -size 200m -fs "Case-sensitive APFS" -volname "SZ Scratch" -type UDIF scratch.dmg
sdev=$(attach scratch.dmg)
XDIR="$W/root/SZ Scratch" check "SZ Case.dmg" "SZ Case"
detach "$sdev"
grep -i 'file system' "out/SZ_Case.dmg-SZ_Case.info" || true
python3 "$TREE" manifest "$W/root/SZ Case" samples/case.json
detach "$dev"

# 3. One container, three volumes: a second plain one and an encrypted one. (A container gets
# one volume per 512 MiB, so it has to be this large.)
hdiutil create -quiet -size 1100m -fs APFS -volname "SZ Multi" -layout GPTSPUD -type UDIF multi-rw.dmg
dev=$(attach multi-rw.dmg)
printf 'first volume\n' > "$W/root/SZ Multi/first.txt"
mkdir -p "$W/root/SZ Multi/Folder" && seq 1 3000 > "$W/root/SZ Multi/Folder/numbers.txt"
container=$(diskutil info -plist "$W/root/SZ Multi" | plutil -extract APFSContainerReference raw -)
echo "container $container"
# A custom mount point needs root: the new volume is mounted where macOS puts it.
diskutil apfs addVolume "$container" APFS "Second" > out/addvolume.log 2>&1 ||
  bad "could not add the second volume: $(tail -2 out/addvolume.log | tr '\n' ' ')"
second_dev=$(diskutil apfs list -plist "$container" | python3 -c '
import plistlib, sys
d = plistlib.load(sys.stdin.buffer)
print([v["DeviceIdentifier"] for c in d["Containers"] for v in c["Volumes"] if v["Name"] == "Second"][0])')
diskutil mount "$second_dev" >> out/addvolume.log 2>&1
second=$(diskutil info -plist "$second_dev" | plutil -extract MountPoint raw -)
echo "second volume $second_dev at $second"
if [ -n "$second" ] && [ -d "$second" ]; then
  printf 'second volume\n' > "$second/second.txt"
  mkdir -p "$second/Dir" && printf 'nested\n' > "$second/Dir/nested.txt"
else
  bad "the second volume did not mount: $(tail -2 out/addvolume.log | tr '\n' ' ')"
fi
diskutil apfs addVolume "$container" APFS "Locked" -passphrase "sz-oracle-test" >> out/addvolume.log 2>&1 ||
  bad "could not add the encrypted volume: $(tail -2 out/addvolume.log | tr '\n' ' ')"
diskutil apfs list "$container" | grep -E 'Name:|Role|FileVault' || true
detach "$dev"
hdiutil convert -quiet multi-rw.dmg -format ULMO -o multi.dmg > out/convert.log 2>&1 ||
  bad "could not convert the three-volume image: $(tail -2 out/convert.log | tr '\n' ' ')"
rm -f multi-rw.dmg
dev=$(attach multi.dmg -readonly)
ls "$W/root"
"$S0" info multi.dmg > out/multi.info 2>&1
cat out/multi.info
check multi.dmg "SZ Multi"
check multi.dmg Second --volume 2
check multi.dmg Second --volume Second
expect_error "encrypted volume" "encrypt" multi.dmg --volume Locked
expect_error "missing volume" "Fourth" multi.dmg --volume Fourth
python3 "$TREE" manifest "$W/root/SZ Multi" samples/multi-1.json
python3 "$TREE" manifest "$W/root/Second" samples/multi-2.json
detach "$dev"

# 4. hdiutil -srcfolder (APFS by default on current macOS).
mkdir -p plain/Folder && printf 'from a folder\n' > plain/readme.txt &&
  seq 1 5000 > plain/Folder/numbers.txt && ln -s readme.txt plain/link
hdiutil create -quiet -srcfolder plain -volname "From Folder" folder.dmg
dev=$(attach folder.dmg -readonly)
check folder.dmg "From Folder"
detach "$dev"

# 5. Small samples for the test suite: a small full tree, the case-sensitive one and the
# container with three volumes.
if FORMAT=ULMO build "SZ Sample" 24m APFS --tiny; then
  dev=$(attach "SZ Sample.dmg" -readonly)
  check "SZ Sample.dmg" "SZ Sample"
  python3 "$TREE" manifest "$W/root/SZ Sample" samples/sample.json
  detach "$dev"
  cp "SZ Sample.dmg" samples/sample.dmg
fi
cp "SZ Case.dmg" samples/case.dmg
cp multi.dmg samples/multi.dmg
ls -l samples

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
