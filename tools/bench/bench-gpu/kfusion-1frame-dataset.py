#!/usr/bin/env python3
"""Build a ONE-FRAME kfusion `.raw` from the ICL-NUIM living-room tarball.

Why this exists
---------------
`docs/known-issues/perf/gpu-osr-refusal-makes-kfusion-8x-slower-20260904.md`
recorded its remaining residual as unmeasurable because "kfusion, whose
dataset and build output are no longer on this box". Half of that was
wrong -- the build output is in `C:/craton/CratonVM1/apps/kfusion-tornadovm`
-- and the other half is fixed by this script, which removes the reason
the dataset was hard to get.

kfusion's own `downloadDataSets.sh` clones slambench and runs
`make deps && make slambench` to convert the ICL-NUIM sequence into the
`.raw` the benchmark reads. That is an apt-installing Linux build, so on
this box it is a non-starter. But the `.raw` layout is simple enough to
write directly, and one frame is all the OSR repro needs:

    per frame:  [int32 w][int32 h]            8 bytes
                [uint16 depth   * w*h]        w*h*2
                [int32 w][int32 h]            8 bytes
                [uint8  r,g,b   * w*h]        w*h*3
                                            = 16 + w*h*5

which for the default 640x480 is exactly 1,536,016 bytes -- the
`head -c 1536016` the known-issue page's repro section already named.

Read out of `RawDevice.java`: `pollDepth` runs before `pollVideo`, each
skips 8 header bytes, depth is read through a `ShortBuffer` with no
scaling, and video is three bytes per pixel.

Usage
-----
    python bench-gpu/kfusion-1frame-dataset.py \
        <living_room_traj2_loop.tgz> <out.raw> [frame_index]

Needs Pillow and numpy. The tarball is ~1.9 GB from
http://www.doc.ic.ac.uk/~ahanda/living_room_traj2_loop.tgz -- members are
`scene_00_NNNN.depth` (text, metres, one value per pixel),
`scene_00_NNNN.png` (RGB) and `scene_00_NNNN.txt` (pose).

One simplification, stated because it matters if you use this for
anything but a JIT/OSR repro: ICL-NUIM `.depth` holds EUCLIDEAN distance
from the camera centre, and slambench's `scene2raw` reprojects it to
z-depth using the camera intrinsics. This script does not -- it converts
metres to millimetres and stops. For reproducing a compile-admission
defect the geometry is irrelevant; for anything that reads the
reconstruction, it is not.
"""

import io
import struct
import sys
import tarfile

import numpy as np
from PIL import Image

W, H = 640, 480  # KfusionConfig's kfusion.raw.width / .height defaults


def build(tgz_path: str, out_path: str, index: int = 0) -> None:
    stem = "scene_00_%04d" % index
    depth_txt = rgb_png = None
    with tarfile.open(tgz_path, "r:gz") as t:
        for m in t:
            if m.name.endswith(stem + ".depth"):
                depth_txt = t.extractfile(m).read()
            elif m.name.endswith(stem + ".png"):
                rgb_png = t.extractfile(m).read()
            if depth_txt is not None and rgb_png is not None:
                break
    if depth_txt is None or rgb_png is None:
        raise SystemExit("frame %d (%s) not found in %s" % (index, stem, tgz_path))

    vals = np.array(depth_txt.split(), dtype=np.float64)
    if vals.size != W * H:
        raise SystemExit("depth has %d values, expected %d" % (vals.size, W * H))
    print("depth: %d values, %.3f..%.3f m (mean %.3f)"
          % (vals.size, vals.min(), vals.max(), vals.mean()))
    mm = np.clip(vals * 1000.0, 0, 65535).astype("<u2")

    img = Image.open(io.BytesIO(rgb_png)).convert("RGB")
    if img.size != (W, H):
        raise SystemExit("png is %s, expected %s" % (img.size, (W, H)))
    rgb = np.asarray(img, dtype=np.uint8).reshape(-1)

    hdr = struct.pack("<ii", W, H)
    frame = hdr + mm.tobytes() + hdr + rgb.tobytes()
    expected = 16 + W * H * 5
    if len(frame) != expected:
        raise SystemExit("frame is %d bytes, expected %d" % (len(frame), expected))

    with open(out_path, "wb") as f:
        f.write(frame)
    print("wrote %s (%d bytes)" % (out_path, len(frame)))


if __name__ == "__main__":
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    build(sys.argv[1], sys.argv[2],
          int(sys.argv[3]) if len(sys.argv) > 3 else 0)
