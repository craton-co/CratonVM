#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Diff the image adjudication of two censuses taken against the same JDK
version built for different platforms.

WHY THIS EXISTS
---------------

Every single-platform census scores `jdk/net/WindowsSocketOptions`,
`java/io/WinNTFileSystem` and `sun/awt/PlatformGraphicsInfo.hasDisplays0` the
same way it scores a genuine dead registration: `ABSENT` or `UNDECL`. They are
not the same thing, and until this diff existed nothing could tell them apart,
so three separate records had to write "needs a Windows-image census" and stop.

They no longer do. CratonVM adjudicates an image it cannot *run* — the pass
parses class bytes off the module image and never executes them — so both
censuses come from one Linux host:

    curl -sL -o win.zip \\
      "https://api.adoptium.net/v3/binary/latest/25/ga/windows/x64/jdk/hotspot/normal/eclipse"
    python3 -c "import zipfile; zipfile.ZipFile('win.zip').extractall('winjdk')"

    for arm in linux:/path/to/linux-jdk windows:winjdk/jdk-25.0.4+7; do
        cratonvm --real-jdk --java-home "${arm#*:}" --explain-jdk-only \\
            --dump-native-registry "census-${arm%%:*}.json" -cp probes L5rProbe
    done
    python3 scripts/jdk-only-platform-diff.py census-linux.json census-windows.json

USE THE SAME JDK VERSION for both arms. A 25.0.3-vs-25.0.4 diff mixes platform
differences with patch-level ones and the output stops meaning what it says.

Result on JDK 25.0.4+7, 2026-08-05: 230 of 11,915 rows differ, and **59 rows
are a genuine ACC_NATIVE bridge only on Windows** — including all nine
`WindowsSocketOptions` entries, the `WinNTFileSystem` family, and
`PlatformGraphicsInfo.hasDisplays0`, whose `JDK-ONLY-CLASSIFY: bridge` marker
was right all along and unprovable on Linux.
"""
import json
import sys
from collections import Counter


def load(path):
    with open(path, encoding="utf-8") as fh:
        doc = json.load(fh)
    if not doc.get("image_adjudication"):
        sys.exit("REFUSING to diff %s: image_adjudication is false. Re-run the "
                 "VM with --explain-jdk-only; without it every "
                 "image_declaring_method is null and the diff would report "
                 "'no differences' for the wrong reason." % path)
    return doc


def verdict(row):
    img = row.get("image_declaring_method") or {}
    if not img.get("image_has_class"):
        return "ABSENT"
    if not img.get("declared"):
        return "UNDECL"
    if img.get("acc_native"):
        return "NATIVE"
    if img.get("has_code"):
        return "CODE"
    return "ABSTRACT"


def main(argv):
    if len(argv) < 3:
        sys.exit(__doc__.strip().splitlines()[0] + "\n\nusage: jdk-only-platform-diff.py "
                 "<census-a.json> <census-b.json> [name-a] [name-b]")
    a_doc, b_doc = load(argv[1]), load(argv[2])
    a_name = argv[3] if len(argv) > 3 else "A"
    b_name = argv[4] if len(argv) > 4 else "B"

    a_rows, b_rows = a_doc["natives"], b_doc["natives"]
    if len(a_rows) != len(b_rows):
        sys.exit("REFUSING: %d rows vs %d. The two censuses must come from the "
                 "same VM binary and the same workload — only --java-home may "
                 "differ, or the rows do not correspond." % (len(a_rows), len(b_rows)))

    print("%-10s %s" % (a_name, a_doc["counts"]))
    print("%-10s %s" % (b_name, b_doc["counts"]))

    moves = Counter()
    differing = []
    for ra, rb in zip(a_rows, b_rows):
        if (ra["class"], ra["name"], ra["descriptor"]) != (rb["class"], rb["name"], rb["descriptor"]):
            sys.exit("REFUSING: row order differs (%s vs %s). Same binary, same "
                     "workload, only --java-home may change."
                     % (ra["class"], rb["class"]))
        va, vb = verdict(ra), verdict(rb)
        if va != vb:
            moves[(va, vb)] += 1
            differing.append((ra, va, vb))

    print("\nrows whose image verdict differs: %d of %d" % (len(differing), len(a_rows)))
    for (va, vb), n in moves.most_common():
        print("  %-16s -> %-16s %5d" % ("%s(%s)" % (va, a_name), "%s(%s)" % (vb, b_name), n))

    for tag, name, sel in (("only on " + b_name, b_name,
                            [(r, va) for r, va, vb in differing if vb == "NATIVE"]),
                           ("only on " + a_name, a_name,
                            [(r, vb) for r, va, vb in differing if va == "NATIVE"])):
        print("\n=== registrations that are a genuine ACC_NATIVE bridge %s (%d) ===" % (tag, len(sel)))
        by_file = Counter((r.get("registered_by") or "?").rsplit(":", 1)[0] for r, _ in sel)
        for f, n in by_file.most_common():
            print("  %5d  %s" % (n, f))

    both_absent = sum(1 for ra, rb in zip(a_rows, b_rows)
                      if verdict(ra) == "ABSENT" and verdict(rb) == "ABSENT")
    print("\nrows ABSENT on BOTH images (dead on every platform, deletion "
          "candidates): %d" % both_absent)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
