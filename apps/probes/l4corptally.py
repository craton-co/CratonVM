import json, glob, os, collections

NOMINATED = {
    ("java/nio/ShortBuffer", "array"), ("java/nio/IntBuffer", "array"),
    ("java/nio/LongBuffer", "array"), ("java/nio/FloatBuffer", "array"),
    ("java/nio/DoubleBuffer", "array"),
    ("java/nio/ShortBuffer", "get"), ("java/nio/LongBuffer", "get"),
    ("java/nio/FloatBuffer", "get"), ("java/nio/DoubleBuffer", "get"),
    ("java/nio/ByteBuffer", "get"), ("java/nio/ByteBuffer", "toString"),
    ("java/nio/HeapCharBuffer", "toString"), ("java/nio/HeapCharBufferR", "toString"),
    ("java/nio/StringCharBuffer", "toString"),
    ("java/nio/ByteBufferAsCharBufferB", "toString"),
    ("java/nio/ByteBufferAsCharBufferL", "toString"),
    ("java/nio/ByteBufferAsCharBufferRB", "toString"),
    ("java/nio/ByteBufferAsCharBufferRL", "toString"),
    ("java/nio/MappedByteBuffer", "force"), ("java/nio/MappedByteBuffer", "load"),
    ("java/nio/MappedByteBuffer", "isLoaded"),
    ("java/nio/file/FileStore", "getBlockSize"),
    ("java/nio/file/SimpleFileVisitor", "preVisitDirectory"),
    ("java/nio/file/SimpleFileVisitor", "visitFile"),
    ("java/nio/file/SimpleFileVisitor", "visitFileFailed"),
    ("java/nio/file/spi/FileSystemProvider", "newFileSystem"),
}

best, where, control = {}, {}, {}
for path in sorted(glob.glob("/data/l4corp/*.json")):
    w = os.path.basename(path)[:-5]
    if w.startswith("rep-"):
        continue
    try:
        d = json.load(open(path))
    except Exception as e:
        print("UNREADABLE", w, e); continue
    io_inv = io_rows = 0
    for r in d["natives"]:
        if r["class"].startswith(("java/io/", "java/nio/")):
            io_inv += r["invocations"]
            io_rows += 1 if r["invocations"] > 0 else 0
        if (r["class"], r["name"]) in NOMINATED:
            k = (r["class"], r["name"], r["descriptor"])
            if r["invocations"] > best.get(k, -1):
                best[k] = r["invocations"]; where[k] = w
    control[w] = (io_inv, io_rows)

print("=" * 72)
print("CONTROL — did these workloads exercise java.io / java.nio at all?")
for w, (inv, rows) in sorted(control.items()):
    print("  %-12s %8d invocations across %4d distinct rows" % (w, inv, rows))
print("=" * 72)
moved = {k: v for k, v in best.items() if v > 0}
still = {k: v for k, v in best.items() if v == 0}
print("nominated rows visible: %d   MOVED off zero: %d   still zero: %d"
      % (len(best), len(moved), len(still)))
print("=" * 72)
if moved:
    print()
    print("=== MOVED — nomination WITHDRAWN for these")
    for k, v in sorted(moved.items(), key=lambda x: -x[1]):
        print("  %-40s %-18s %-26s inv=%-6s in %s"
              % (k[0], k[1], k[2], v, where[k]))
else:
    print()
    print("=== none moved")
