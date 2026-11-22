# Compares the output directories of the C++ driver and the Rust driver for one command.
# Usage: python -I compare.py <name> <cppdir> <rustdir> <ffmpeg>
# Exit code 0 = every result line and every output file identical (all parts count).
import os, subprocess, sys, tempfile
import numpy as np

name, cpp, rust, ffmpeg = sys.argv[1:5]


def lines(d):
    p = os.path.join(d, "results.txt")
    if not os.path.exists(p):
        return None
    with open(p, "rb") as f:
        return [l.rstrip(b"\r\n").decode("utf-8", "replace") for l in f.read().split(b"\n") if l.strip(b"\r")]


a, b = lines(cpp), lines(rust)
if a is None or b is None:
    print(f"[{name}] MISSING results.txt (cpp={a is not None} rust={b is not None})")
    sys.exit(1)

bad = 0
only = lambda s: s.split(" ")[0]
bd = {only(l): l for l in b}
ad = {only(l): l for l in a}
for l in a:
    k = only(l)
    if k not in bd:
        print(f"[{name}] only in C++ : {l}")
        bad += 1
    elif bd[k] != l:
        bad += 1
        print(f"[{name}] result line differs:\n    C++ : {l}\n    Rust: {bd[k]}")
for l in b:
    if only(l) not in ad:
        print(f"[{name}] only in Rust: {l}")
        bad += 1
print(f"[{name}] result lines: {len(a)} C++ / {len(b)} Rust, identical: {sum(1 for l in a if bd.get(only(l)) == l)}")


def planes(raw, w, h):
    y = raw[: w * h].reshape(h, w)
    uv = raw[w * h : w * h * 3 // 2]
    return y, uv


def size_of(fname):
    # ..._<w>x<h>.nv12 (or ..._<w>x<h>_<suffix>)
    import re
    m = re.findall(r"(\d+)x(\d+)", fname)
    return (int(m[-1][0]), int(m[-1][1])) if m else (None, None)


files = sorted(f for f in os.listdir(cpp) if f.endswith(".nv12") or f.endswith(".bgra") or f.endswith(".png"))
same = diff = 0
for f in files:
    pa, pb = os.path.join(cpp, f), os.path.join(rust, f)
    if not os.path.exists(pb):
        print(f"[{name}] {f}: missing in Rust output")
        bad += 1
        continue
    if f.endswith(".png"):
        outs = []
        for p in (pa, pb):
            out = os.path.join(tempfile.gettempdir(), "fx_cmp_" + os.path.basename(os.path.dirname(p)) + "_" + os.path.basename(os.path.dirname(os.path.dirname(p))) + ".raw")
            subprocess.run([ffmpeg, "-y", "-loglevel", "error", "-i", p, "-pix_fmt", "bgra", "-f", "rawvideo", out], check=True)
            outs.append(np.fromfile(out, np.uint8))
            os.remove(out)
        ea = np.array_equal(outs[0], outs[1])
        print(f"[{name}] {f}: decoded pixels {'IDENTICAL' if ea else 'DIFFERENT'} ({outs[0].size} bytes); file bytes {'identical' if open(pa,'rb').read()==open(pb,'rb').read() else 'differ'}")
        bad += 0 if ea else 1
        continue
    da, db = np.fromfile(pa, np.uint8), np.fromfile(pb, np.uint8)
    if da.shape != db.shape:
        print(f"[{name}] {f}: size differs {da.shape} vs {db.shape}")
        bad += 1
        continue
    if np.array_equal(da, db):
        same += 1
        continue
    diff += 1
    bad += 1
    delta = np.abs(da.astype(np.int16) - db.astype(np.int16))
    msg = f"[{name}] {f}: DIFFERENT max={delta.max()} mean={delta.mean():.5f} differing={100.0 * np.count_nonzero(delta) / delta.size:.4f}% of bytes"
    w, h = size_of(f)
    if f.endswith(".nv12") and w and h and da.size == w * h * 3 // 2:
        dy, duv = delta[: w * h], delta[w * h :]
        msg += f" | Y max={dy.max()} mean={dy.mean():.5f} diff={100.0*np.count_nonzero(dy)/dy.size:.4f}% | UV max={duv.max()} mean={duv.mean():.5f} diff={100.0*np.count_nonzero(duv)/duv.size:.4f}%"
        ys, xs = np.nonzero(dy.reshape(h, w))
        if ys.size:
            msg += f" | Y diff box x {xs.min()}..{xs.max()} y {ys.min()}..{ys.max()}"
    print(msg)
print(f"[{name}] output files: {same} byte-identical, {diff} different, {len(files)} total")
print(f"[{name}] RESULT: {'IDENTICAL' if bad == 0 else 'DIFFERENCES: %d' % bad}")
sys.exit(0 if bad == 0 else 1)
