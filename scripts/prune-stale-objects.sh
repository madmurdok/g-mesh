#!/usr/bin/env bash
# Deletes the `*.rcgu.o` files in <target>/debug/deps that no current
# executable or dylib references.
#
#   scripts/prune-stale-objects.sh [--dry-run]
#
# Why: under macOS's default `split-debuginfo=unpacked`, every recompile of a
# crate writes a new set of `.rcgu.o` (new per-compilation suffix) next to the
# old one, and neither cargo nor rustc deletes the old set. Linked Mach-O files
# keep debug-map (N_OSO) entries naming the objects of their own build; that
# is where backtraces get file:line. So the objects some executable names are
# kept, exactly, and everything else is a leftover. See
# docs/results/gm-538-deps-growth.md.
#
# Honours CARGO_TARGET_DIR. Do not run it while a build writes to the same
# target dir: objects of a compile that has not linked yet look unreferenced.
# On other operating systems it does nothing.
set -euo pipefail

dry_run=0
case "${1:-}" in
"") ;;
--dry-run) dry_run=1 ;;
*)
	echo "usage: scripts/prune-stale-objects.sh [--dry-run]" >&2
	exit 2
	;;
esac

if [ "$(uname -s)" != Darwin ]; then
	echo "prune-stale-objects: not macOS, nothing to do"
	exit 0
fi

cd "$(dirname "$0")/.."
target="${CARGO_TARGET_DIR:-target}"
if [ ! -d "$target/debug/deps" ]; then
	echo "prune-stale-objects: no $target/debug/deps, nothing to do"
	exit 0
fi

exec python3 - "$target/debug" "$dry_run" <<'PY'
import mmap
import os
import struct
import sys

debug, dry_run = sys.argv[1], sys.argv[2] == "1"
deps = os.path.join(debug, "deps")
MH_MAGIC_64 = b"\xcf\xfa\xed\xfe"
FAT_MAGICS = (b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca")
LC_SYMTAB = 0x2
N_OSO = 0x66
SKIP_EXT = (".o", ".d", ".rlib", ".rmeta")


def oso_paths(path):
    """The N_OSO strings of a thin 64-bit little-endian Mach-O file."""
    with open(path, "rb") as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as m:
        ncmds = struct.unpack_from("<I", m, 16)[0]
        off = 32
        for _ in range(ncmds):
            cmd, size = struct.unpack_from("<II", m, off)
            if cmd == LC_SYMTAB:
                symoff, nsyms, stroff, _ = struct.unpack_from("<IIII", m, off + 8)
                break
            off += size
        else:
            return []
        table = m[symoff : symoff + nsyms * 16]
        types = table[4::16]  # n_type of every nlist_64 entry
        out = []
        i = types.find(N_OSO)
        while i != -1:
            strx = struct.unpack_from("<I", table, i * 16)[0]
            start = stroff + strx
            out.append(m[start : m.find(b"\0", start)].decode("utf-8", "replace"))
            i = types.find(N_OSO, i + 1)
        return out


seen = set()
scanned = 0
referenced = set()
for d in (debug, deps, os.path.join(debug, "examples")):
    if not os.path.isdir(d):
        continue
    for entry in os.scandir(d):
        if entry.name.endswith(SKIP_EXT) or not entry.is_file(follow_symlinks=False):
            continue
        st = entry.stat(follow_symlinks=False)
        if (st.st_dev, st.st_ino) in seen:  # debug/<bin> hardlinks deps/<bin>-<hash>
            continue
        seen.add((st.st_dev, st.st_ino))
        with open(entry.path, "rb") as f:
            magic = f.read(4)
        if magic in FAT_MAGICS:
            sys.exit(f"prune-stale-objects: universal binary {entry.path} is not supported; nothing deleted")
        if magic != MH_MAGIC_64:
            continue
        scanned += 1
        for p in oso_paths(entry.path):
            # By basename, and only loose objects in a `deps` dir: an
            # `x.rlib(member.o)` entry is read from the rlib. The basename also
            # matches when the target dir was copied or moved after linking.
            if "(" not in p and os.path.basename(os.path.dirname(p)) == "deps":
                referenced.add(os.path.basename(p))

objects = [e.name for e in os.scandir(deps) if e.name.endswith(".rcgu.o")]
stale = [n for n in objects if n not in referenced]
kept = len(objects) - len(stale)
deleted = 0
if not dry_run:
    for n in stale:
        try:
            os.unlink(os.path.join(deps, n))
            deleted += 1
        except FileNotFoundError:
            pass
print(
    f"prune-stale-objects: {scanned} Mach-O files scanned, {len(objects)} .rcgu.o in {deps}: "
    f"{kept} referenced, {len(stale)} stale, {deleted} deleted" + (" (dry run)" if dry_run else "")
)
PY
