import java.nio.ByteBuffer;

/**
 * Differential verifier for the five bulk `ByteBuffer` natives that
 * `perf(nio): bulk-copy the ByteBuffer natives` rewrote — `get([B)`,
 * `get([BII)`, `put([B)`, `put([BII)` and `put(Ljava/nio/ByteBuffer;)`.
 *
 * Those natives moved from a per-element accessor loop to the VM's memcpy
 * intrinsics. The intrinsics bounds-check independently and decline (writing
 * nothing) rather than partially copying, so the risk is not a slow path but a
 * SILENTLY DIFFERENT one: a case where the loop copied and the intrinsic
 * declines, or where offsets are applied differently.
 *
 * Prints one FNV-1a checksum over every byte produced plus every exception
 * type thrown. Run under HotSpot and CratonVM; the two must match exactly.
 * A checksum is used rather than per-case printing so that a single divergent
 * byte anywhere cannot be overlooked.
 */
public class ByteBufferBulkProbe {

    private static long h = 0xcbf29ce484222325L;

    private static void mix(long v) {
        h ^= v;
        h *= 0x100000001b3L;
    }

    private static void mixBuf(ByteBuffer b) {
        mix(b.position());
        mix(b.limit());
        mix(b.capacity());
        // Read the WHOLE backing store, but through a duplicate that has been
        // cleared so every index is inside the limit. Absolute `get(int)` is
        // bounds-checked against the LIMIT, not the capacity, so reading to
        // capacity on the original throws on a flipped buffer. (Until
        // 2026-08-05 CratonVM returned 0 there instead of throwing, which made
        // an earlier version of this probe report a divergence in `put([BII)`
        // that does not exist. The `absGetPastLimit` family at the bottom of
        // `main` now tests that behaviour deliberately; this helper must stay
        // inside the limit so it keeps measuring the bulk paths only.)
        // See fixed-suite-bugs/bytebuffer-jdk-contract-divergences-20260731-FIXED.md.
        ByteBuffer all = b.duplicate();
        all.clear();
        for (int i = 0; i < all.capacity(); i++) {
            mix(all.get(i));
        }
    }

    private static void mixArr(byte[] a) {
        mix(a.length);
        for (byte v : a) {
            mix(v);
        }
    }

    private static void note(String tag) {
        for (int i = 0; i < tag.length(); i++) {
            mix(tag.charAt(i));
        }
    }

    interface Case {
        void run() throws Exception;
    }

    private static final boolean VERBOSE = System.getProperty("probe.verbose") != null;

    private static void guarded(String tag, Case c) {
        note(tag);
        String outcome;
        try {
            c.run();
            outcome = "ok";
        } catch (Throwable t) {
            // Type only: messages are deliberately allowed to differ.
            outcome = t.getClass().getName();
        }
        note(outcome);
        if (VERBOSE) {
            System.out.println("case " + tag + " -> " + outcome + " h=" + h);
        }
    }

    private static byte[] pattern(int n, int seed) {
        byte[] a = new byte[n];
        for (int i = 0; i < n; i++) {
            a[i] = (byte) (i * 31 + seed);
        }
        return a;
    }

    public static void main(String[] args) {
        int[] sizes = { 0, 1, 7, 8, 63, 64, 4096, 8192 };

        for (int size : sizes) {
            // --- put([B) / get([B) round trip on a heap buffer
            guarded("putB/" + size, () -> {
                ByteBuffer b = ByteBuffer.allocate(size);
                b.put(pattern(size, 1));
                b.flip();
                byte[] out = new byte[size];
                b.get(out);
                mixBuf(b);
                mixArr(out);
            });

            // --- put([BII) / get([BII) with a non-zero offset
            guarded("putBII/" + size, () -> {
                int off = size > 8 ? 3 : 0;
                int len = Math.max(0, size - off);
                ByteBuffer b = ByteBuffer.allocate(size);
                b.put(pattern(size, 2), off, len);
                b.flip();
                byte[] out = new byte[size];
                b.get(out, off, len);
                mixBuf(b);
                mixArr(out);
            });

            // --- put(ByteBuffer) heap -> heap
            guarded("putBB/" + size, () -> {
                ByteBuffer src = ByteBuffer.wrap(pattern(size, 3));
                ByteBuffer dst = ByteBuffer.allocate(size);
                dst.put(src);
                mixBuf(src);
                mixBuf(dst);
            });

            // --- put(ByteBuffer) direct -> heap and heap -> direct
            guarded("putBBdirect/" + size, () -> {
                ByteBuffer src = ByteBuffer.allocateDirect(size);
                src.put(pattern(size, 4));
                src.flip();
                ByteBuffer dst = ByteBuffer.allocate(size);
                dst.put(src);
                mixBuf(dst);

                ByteBuffer src2 = ByteBuffer.wrap(pattern(size, 5));
                ByteBuffer dst2 = ByteBuffer.allocateDirect(size);
                dst2.put(src2);
                dst2.flip();
                byte[] back = new byte[size];
                dst2.get(back);
                mixArr(back);
            });

            // --- SLICE: a heap buffer with a non-zero array offset, which is
            // where `s2_bb_heap_base` participates and an off-by-one would hide
            if (size >= 8) {
                guarded("slice/" + size, () -> {
                    ByteBuffer base = ByteBuffer.allocate(size);
                    base.position(4);
                    ByteBuffer sl = base.slice();
                    sl.put(pattern(sl.capacity(), 6));
                    sl.flip();
                    byte[] out = new byte[sl.capacity()];
                    sl.get(out);
                    mixArr(out);
                    mixBuf(base);

                    // slice as the SOURCE of put(ByteBuffer)
                    ByteBuffer dst = ByteBuffer.allocate(sl.capacity());
                    sl.rewind();
                    dst.put(sl);
                    mixBuf(dst);
                });
            }

            // --- read-only source
            guarded("readonly/" + size, () -> {
                ByteBuffer src = ByteBuffer.wrap(pattern(size, 7)).asReadOnlyBuffer();
                ByteBuffer dst = ByteBuffer.allocate(size);
                dst.put(src);
                mixBuf(dst);
            });
        }

        // --- bounds / negative cases: every one must throw the same type
        guarded("overflow", () -> {
            ByteBuffer dst = ByteBuffer.allocate(4);
            dst.put(pattern(8, 8));
        });
        guarded("underflow", () -> {
            ByteBuffer src = ByteBuffer.allocate(4);
            src.flip();
            src.get(new byte[8]);
        });
        guarded("negOff", () -> ByteBuffer.allocate(8).put(pattern(8, 9), -1, 4));
        guarded("negLen", () -> ByteBuffer.allocate(8).put(pattern(8, 10), 0, -4));
        guarded("offPastEnd", () -> ByteBuffer.allocate(8).put(pattern(8, 11), 6, 4));
        guarded("getNegOff", () -> ByteBuffer.allocate(8).get(new byte[8], -1, 4));
        guarded("putIntoReadOnly", () -> {
            ByteBuffer ro = ByteBuffer.allocate(8).asReadOnlyBuffer();
            ro.put(pattern(8, 12));
        });
        guarded("selfPut", () -> {
            ByteBuffer b = ByteBuffer.wrap(pattern(16, 13));
            b.put(b);
        });

        // --- ABSOLUTE accessors are bounds-checked against the LIMIT, not the
        // capacity (`java.nio.Buffer.checkIndex`). Every case below sits inside
        // the capacity and outside the limit, so a capacity-only check — or no
        // check at all — reads back a plausible zero instead of throwing. That
        // silent-wrong-answer shape is the whole reason these are here; see
        // fixed-suite-bugs/bytebuffer-jdk-contract-divergences-20260731-FIXED.md.
        guarded("absGetPastLimit", () -> {
            ByteBuffer b = ByteBuffer.allocate(8);
            b.put(new byte[] { 1, 2, 3, 4 });
            b.flip(); // limit=4, capacity=8
            mix(b.get(6));
        });
        guarded("absPutPastLimit", () -> {
            ByteBuffer b = ByteBuffer.allocate(8);
            b.put(new byte[] { 1, 2, 3, 4 });
            b.flip();
            b.put(6, (byte) 9);
        });
        guarded("absGetNegative", () -> mix(ByteBuffer.allocate(8).get(-1)));
        guarded("absPutNegative", () -> ByteBuffer.allocate(8).put(-1, (byte) 1));
        guarded("absGetAtLimit", () -> {
            ByteBuffer b = ByteBuffer.allocate(8);
            b.limit(4);
            mix(b.get(4)); // index == limit is already out of range
        });
        // The multi-byte absolute forms check `index + width <= limit`, so an
        // index that is itself in range still throws when the read would run
        // off the end.
        guarded("absGetIntStraddlingLimit", () -> {
            ByteBuffer b = ByteBuffer.allocate(8);
            b.limit(6);
            mix(b.getInt(4)); // needs [4,8), limit is 6
        });
        guarded("absGetShortStraddlingLimit", () -> {
            ByteBuffer b = ByteBuffer.allocate(8);
            b.limit(5);
            mix(b.getShort(4)); // needs [4,6), limit is 5
        });
        guarded("absGetLongStraddlingLimit", () -> {
            ByteBuffer b = ByteBuffer.allocate(16);
            b.limit(12);
            mix(b.getLong(8)); // needs [8,16), limit is 12
        });
        guarded("absPutIntStraddlingLimit", () -> {
            ByteBuffer b = ByteBuffer.allocate(8);
            b.limit(6);
            b.putInt(4, 0x01020304);
        });
        // ...and the in-range forms must still WORK, so the new gate cannot be
        // "throw on everything absolute".
        guarded("absInRangeRoundTrip", () -> {
            ByteBuffer b = ByteBuffer.allocate(8);
            b.putInt(0, 0x01020304);
            b.putInt(4, 0x05060708);
            mix(b.getInt(0));
            mix(b.getInt(4));
            mix(b.get(7));
            mixBuf(b);
        });
        // A DIRECT receiver takes a different storage path through the same
        // accessors; it must agree.
        guarded("absDirectPastLimit", () -> {
            ByteBuffer b = ByteBuffer.allocateDirect(8);
            b.putInt(0, 0x0a0b0c0d);
            b.limit(4);
            mix(b.getInt(0));
            mix(b.get(6));
        });

        System.out.println("ByteBufferBulkProbe checksum=" + h);
    }
}
