// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w5/oldcompact5 (2026-09-24): the JIT-WARM shape of
 * {@code GenR4W4HumongousFragProbe}. The humongous request that needs a
 * compaction is made from a method that has been compiled, so the collection
 * that answers it runs with a live compiled frame on the stack. That is the
 * collection the old-gen compaction around pinned objects
 * ({@code CRATONVM_GC_OLD_PINNED_COMPACT}) exists for, and the one in which the
 * interpreter's conservative frame probe is engaged (its words are not
 * published, so the pin set falls back to every root). Page:
 * {@code docs/internal/gc/gengc-r4w4-oom-humongous-on-fragmented-old-gen-FIXED-20260924.md}.
 *
 * <p>Geometry at {@code -Xmx128m}: 32 MiB semi-spaces, a 64 MiB old
 * generation, humongous threshold 16 MiB. Three 17 MiB arrays go to old gen
 * back to back; the first and third are dropped and the middle one is kept in a
 * static field. A 33 MiB request then fits no hole until the kept array moves.
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx128m}) prints:
 * <pre>
 *   jit-warm-humongous-after-fragmentation ok
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench GenR4W5OldPinnedCompactProbe
 *   CRATONVM_GC_OLD_PINNED_COMPACT=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W5OldPinnedCompactProbe
 * </pre>
 * CratonVM, with the flag: {@code ok} when the kept array is not pinned. While
 * the interpreter's conservative probe words are unpublished, a collection
 * with a live compiled frame pins every root, the static's referent included,
 * and the line reads {@code FAILED: OutOfMemoryError "Java heap space"} (unless
 * the static was deferred to {@code metadata_pin} and so never entered the root
 * slice); see the root-gatherer request in
 * {@code docs/internal/reviews/gengc-round4-w5-oldcompact5-20260924.md}.
 */
public final class GenR4W5OldPinnedCompactProbe {
    static final int MIB = 1 << 20;
    static volatile Object sink;
    static byte[] keep;

    /** Allocates A, B, C back to back; keeps only B. Own frame: no dead local roots A or C. */
    static void fragment() {
        byte[] a = new byte[17 * MIB];
        byte[] b = new byte[17 * MIB];
        byte[] c = new byte[17 * MIB];
        a[0] = 1;
        c[0] = 1;
        b[b.length - 1] = 7;
        sink = a;
        sink = c;
        sink = null;
        keep = b;
    }

    /** The allocation site; warmed until it is compiled. */
    static byte[] allocate(int bytes) {
        byte[] d = new byte[bytes];
        d[bytes - 1] = 1;
        return d;
    }

    public static void main(String[] args) {
        long warm = 0;
        for (int i = 0; i < 50_000; i++) {
            warm += allocate(16 + (i & 63)).length;
        }
        sink = null;
        fragment();
        boolean ok;
        try {
            byte[] d = allocate(33 * MIB);
            ok = d[d.length - 1] == 1 && keep[keep.length - 1] == 7 && warm > 0;
            System.out.println("jit-warm-humongous-after-fragmentation "
                    + (ok ? "ok" : "FAILED: data corrupted"));
        } catch (OutOfMemoryError e) {
            ok = false;
            System.out.println("jit-warm-humongous-after-fragmentation FAILED: OutOfMemoryError \""
                    + e.getMessage() + "\"");
        }
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
