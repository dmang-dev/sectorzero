#!/usr/bin/env python3
"""Helper for .github/oracle/apfs.sh (macOS only).

  apfs_tree.py populate DIR [--small]     fill a mounted APFS volume with test files
  apfs_tree.py manifest DIR OUT.json      describe a folder tree (what s0 must extract)
  apfs_tree.py compare REF OUT [--forks]  compare s0's extraction OUT with the mounted REF
  apfs_tree.py decmpfs DIR                count files by decmpfs compression type

The tree covers what an APFS reader has to get right: many files (a B-tree of several
levels), large, sparse and fragmented files, files rewritten and deleted after a sync (older
object versions), symlinks, hard links, extended attributes (inline and as streams), resource
forks, Finder info, the hidden flag, Unicode names, deep folders and files compressed by
afsctool (zlib, LZVN, LZFSE) and ditto.
"""

import ctypes
import ctypes.util
import hashlib
import json
import os
import subprocess
import sys
import unicodedata

IGNORED = {
    ".fseventsd",
    ".Trashes",
    ".Spotlight-V100",
    ".DS_Store",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
}

libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
libc.getxattr.restype = ctypes.c_ssize_t
libc.getxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p,
                          ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
libc.listxattr.restype = ctypes.c_ssize_t
libc.listxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_size_t, ctypes.c_int]
XATTR_NOFOLLOW = 0x0001
XATTR_SHOWCOMPRESSION = 0x0020


def xattr_names(path, options=XATTR_NOFOLLOW):
    p = os.fsencode(path)
    n = libc.listxattr(p, None, 0, options)
    if n <= 0:
        return []
    buf = ctypes.create_string_buffer(n)
    n = libc.listxattr(p, buf, n, options)
    return [x.decode() for x in buf.raw[:n].split(b"\0") if x]


def xattr(path, name, options=XATTR_NOFOLLOW):
    p, k = os.fsencode(path), name.encode()
    n = libc.getxattr(p, k, None, 0, 0, options)
    if n < 0:
        return None
    buf = ctypes.create_string_buffer(max(n, 1))
    n = libc.getxattr(p, k, buf, n, 0, options)
    return buf.raw[:n] if n >= 0 else None


def run(*args):
    subprocess.run(args, check=True, stdout=subprocess.DEVNULL)


def write(root, rel, data):
    path = os.path.join(root, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    return path


def populate(root, small):
    def numbers(n):
        return "".join(f"{i}\n" for i in range(1, n + 1)).encode()

    write(root, "readme.txt", b"SectorZero APFS oracle\n")
    write(root, "Folder/numbers.txt", numbers(2000 if small else 20000))
    write(root, "Folder/Sub/deep.txt", b"deep\n")
    write(root, "empty.txt", b"")
    write(root, "random.bin", os.urandom((40 if small else 4096) * 1024))
    for name in ["café.txt", "naïve.txt", "日本語.txt",
                 "\U0001f600 smile.txt", "L" * 200 + ".txt", "with space .txt"]:
        write(root, name, name.encode() + b"\n")
    deep = "/".join(f"d{i}" for i in range(1, 21))
    write(root, f"{deep}/leaf.txt", b"leaf\n")

    # Sparse files: a hole in the middle, and one at the end.
    with open(os.path.join(root, "sparse.bin"), "wb") as f:
        f.write(b"start")
        f.seek((1 if small else 8) * 1024 * 1024)
        f.write(b"end")
    with open(os.path.join(root, "tail-hole.bin"), "wb") as f:
        f.write(b"x" * 5000)
        f.truncate(3 * 1024 * 1024)

    # Files that grow in turns, synced each time, so they end up in several extents.
    rounds = 4 if small else 12
    files = [open(os.path.join(root, f"frag-{c}.bin"), "wb") for c in "abc"]
    for _ in range(rounds):
        for f in files:
            f.write(os.urandom(16384 if small else 65536))
            f.flush()
            os.fsync(f.fileno())
    for f in files:
        f.close()

    # Older versions: a file rewritten and one deleted after a sync.
    write(root, "changed.txt", b"version 1, a little longer than the second\n")
    write(root, "deleted.txt", b"gone\n")
    os.sync()
    write(root, "changed.txt", b"version 2\n")
    os.remove(os.path.join(root, "deleted.txt"))

    # Links.
    os.symlink("readme.txt", os.path.join(root, "link-readme"))
    os.symlink("Folder", os.path.join(root, "link-folder"))
    os.symlink("nowhere/missing", os.path.join(root, "link-dangling"))
    os.symlink("/etc/hosts", os.path.join(root, "link-absolute"))
    os.link(os.path.join(root, "readme.txt"), os.path.join(root, "hard-readme.txt"))
    os.link(os.path.join(root, "Folder/numbers.txt"),
            os.path.join(root, "Folder/Sub/hard-numbers.txt"))

    # Extended attributes, resource forks, Finder info, flags and a fixed date.
    p = write(root, "xattrs.txt", b"has attributes\n")
    run("xattr", "-w", "user.note", "hello", p)
    run("xattr", "-w", "user.big", "x" * 20000, p)
    p = write(root, "forked.txt", b"data fork\n")
    run("xattr", "-wx", "com.apple.ResourceFork", os.urandom(3000).hex(), p)
    p = write(root, "big-fork.txt", b"data fork with a large resource fork\n")
    run("xattr", "-wx", "com.apple.ResourceFork", os.urandom(200000).hex(), p)
    p = write(root, "typed.txt", b"TEXT ttxt\n")
    run("xattr", "-wx", "com.apple.FinderInfo", (b"TEXTttxt" + bytes(24)).hex(), p)
    p = write(root, "hidden.txt", b"hidden\n")
    run("chflags", "hidden", p)
    p = write(root, "old.txt", b"from 2000\n")
    os.utime(p, ns=(946_684_800_123_456_789, 946_684_800_123_456_789))

    # Many files in one folder: several levels of B-tree.
    for i in range(300 if small else 5000):
        write(root, f"many/file {i:05}.txt", f"file {i}\n".encode())

    # Compressed files: a small one (kept in the attribute) and a large one (in the resource
    # fork, in 64 KiB chunks), with each compressor afsctool has, and ditto's default.
    text = numbers(30000 if small else 200000)
    for algo in ["ZLIB", "LZVN", "LZFSE"]:
        d = os.path.join(root, f"comp-{algo.lower()}")
        write(root, f"comp-{algo.lower()}/small.txt", b"small and compressible " * 20)
        write(root, f"comp-{algo.lower()}/text.txt", text)
        write(root, f"comp-{algo.lower()}/random.bin", os.urandom(70000))
        r = subprocess.run(["afsctool", "-c", "-T", algo, d], capture_output=True, text=True)
        if r.returncode != 0:
            print(f"note: afsctool -T {algo}: {r.stderr.strip()}", file=sys.stderr)
    src = os.path.join(os.path.dirname(os.path.abspath(root)), "ditto-src")
    write(src, "small.txt", b"small and compressible " * 20)
    write(src, "text.txt", text)
    run("ditto", "--hfsCompression", src, os.path.join(root, "comp-ditto"))
    os.sync()


def walk(root):
    """(relative path, lstat) for everything under root, parents first."""
    out = []
    for dirpath, dirs, files in os.walk(root):
        rel = os.path.relpath(dirpath, root)
        if rel == ".":
            dirs[:] = [d for d in dirs if d not in IGNORED]
            files = [f for f in files if f not in IGNORED]
        dirs.sort()
        for name in sorted(dirs) + sorted(files):
            p = os.path.join(dirpath, name)
            out.append((os.path.normpath(os.path.join(rel, name)), os.lstat(p), p))
    return out


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def describe(root):
    m = {}
    for rel, st, p in walk(root):
        key = unicodedata.normalize("NFC", rel)
        if os.path.islink(p):
            e = {"type": "link", "target": os.readlink(p)}
        elif os.path.isdir(p):
            e = {"type": "dir"}
        else:
            e = {"type": "file", "size": st.st_size, "sha256": sha(p),
                 "mtime": st.st_mtime_ns // 1_000_000_000,
                 "hidden": bool(st.st_flags & 0x8000)}
            rsrc = xattr(p, "com.apple.ResourceFork")
            if rsrc:
                e["rsrc_sha256"] = hashlib.sha256(rsrc).hexdigest()
            finder = xattr(p, "com.apple.FinderInfo")
            if finder and any(finder):
                e["finder"] = finder.hex()
        m[key] = e
    return m


def appledouble(path):
    """The Finder info and resource fork in an AppleDouble file."""
    with open(path, "rb") as f:
        b = f.read()
    if b[:8] != bytes.fromhex("0005160700020000"):
        return None, None
    finder = rsrc = None
    n = int.from_bytes(b[24:26], "big")
    for i in range(n):
        eid, off, ln = (int.from_bytes(b[26 + i * 12 + k:30 + i * 12 + k], "big")
                        for k in (0, 4, 8))
        if eid == 9:
            finder = b[off:off + 32]
        elif eid == 2:
            rsrc = b[off:off + ln]
    return finder, rsrc


def compare(ref, out, forks):
    want = describe(ref)
    got = {}
    doubles = {}
    for rel, st, p in walk(out):
        key = unicodedata.normalize("NFC", rel)
        base = os.path.basename(key)
        if forks and base.startswith("._") and os.path.isfile(p):
            doubles[os.path.join(os.path.dirname(key), base[2:])] = appledouble(p)
            continue
        if os.path.islink(p):
            got[key] = {"type": "link", "target": os.readlink(p)}
        elif os.path.isdir(p):
            got[key] = {"type": "dir"}
        else:
            got[key] = {"type": "file", "size": st.st_size, "sha256": sha(p),
                        "mtime": st.st_mtime_ns // 1_000_000_000}
    problems = []
    for k in sorted(set(want) - set(got)):
        problems.append(f"missing: {k}")
    for k in sorted(set(got) - set(want)):
        problems.append(f"extra: {k}")
    for k in sorted(set(want) & set(got)):
        w, g = want[k], got[k]
        if w["type"] != g["type"]:
            problems.append(f"{k}: {g['type']}, should be {w['type']}")
        elif w["type"] == "link" and w["target"] != g["target"]:
            problems.append(f"{k}: link to {g['target']!r}, should be {w['target']!r}")
        elif w["type"] == "file":
            if w["sha256"] != g["sha256"]:
                problems.append(f"{k}: contents differ ({g['size']} bytes, should be {w['size']})")
            elif w["mtime"] != g["mtime"]:
                problems.append(f"{k}: modified {g['mtime']}, should be {w['mtime']}")
            if forks and ("rsrc_sha256" in w or "finder" in w):
                finder, rsrc = doubles.get(k, (None, None))
                if finder is None:
                    problems.append(f"{k}: no AppleDouble file")
                else:
                    if w.get("finder", "00" * 32) != finder.hex():
                        problems.append(f"{k}: Finder info {finder.hex()}, should be {w.get('finder')}")
                    rs = hashlib.sha256(rsrc or b"").hexdigest() if rsrc else None
                    if w.get("rsrc_sha256") != rs:
                        problems.append(f"{k}: resource fork differs")
    for p in problems[:25]:
        print(p)
    if len(problems) > 25:
        print(f"... {len(problems) - 25} more")
    files = sum(1 for e in want.values() if e["type"] == "file")
    print(f"{len(want)} entries ({files} files), {len(problems)} problem(s)")
    return 1 if problems else 0


def decmpfs(root):
    counts = {}
    for rel, st, p in walk(root):
        if os.path.isfile(p) and not os.path.islink(p) and st.st_flags & 0x20:
            h = xattr(p, "com.apple.decmpfs", XATTR_NOFOLLOW | XATTR_SHOWCOMPRESSION)
            t = int.from_bytes(h[4:8], "little") if h and len(h) >= 16 else -1
            counts[t] = counts.get(t, 0) + 1
    print("decmpfs types: " + (", ".join(f"type {t}: {n}" for t, n in sorted(counts.items()))
                               or "none"))


def main():
    cmd, args = sys.argv[1], sys.argv[2:]
    if cmd == "populate":
        populate(args[0], "--small" in args)
    elif cmd == "manifest":
        with open(args[1], "w", encoding="utf-8") as f:
            json.dump(describe(args[0]), f, indent=1, ensure_ascii=False, sort_keys=True)
    elif cmd == "compare":
        sys.exit(compare(args[0], args[1], "--forks" in args))
    elif cmd == "decmpfs":
        decmpfs(args[0])
    else:
        sys.exit(f"unknown command {cmd}")


if __name__ == "__main__":
    main()
