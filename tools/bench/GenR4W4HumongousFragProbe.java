// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w4/oom (2026-09-24), filed, NOT fixed: a humongous array must not fail
 * with {@code OutOfMemoryError} while the old generation has enough free space
 * in total but no contiguous block. HotSpot Serial's full collection is a
 * mark-compact and slides the survivors together; CratonVM's Generational old
 * generation compacts only under the opt-in {@code oldgen_compact_enabled()}
 * (default OFF since 2026-08-03), so its last-ditch major frees the space in
 * place and the request still finds no hole. Page:
 * {@code docs/internal/gc/gengc-r4w4-oom-humongous-on-fragmented-old-gen-FIXED-20260924.md}.
 *
 * <p>Geometry at {@code -Xmx128m}: 32 MiB semi-spaces, a 64 MiB old
 * generation, humongous threshold 16 MiB. Three 17 MiB arrays go to old gen
 * back to back; the first and third are dropped and the middle one kept. Old
 * gen then has about 47 MiB free, in a 17 MiB hole and a trailing block of
 * about 30 MiB. A 33 MiB request fits neither hole, nor the 32 MiB young
 * semi-space it would fall back to. After compaction it fits easily (live data
 * is 17 + 33 MiB of a 128 MiB heap).
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx128m}) prints:
 * <pre>
 *   humongous-after-fragmentation ok
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench GenR4W4HumongousFragProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W4HumongousFragProbe
 * </pre>
 * CratonVM is expected to print {@code humongous-after-fragmentation FAILED:
 * OutOfMemoryError "Java heap space"} until the page is fixed.
 */
public final class GenR4W4HumongousFragProbe {
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

    public static void main(String[] args) {
        fragment();
        boolean ok;
        try {
            byte[] d = new byte[33 * MIB];
            d[d.length - 1] = 1;
            ok = keep[keep.length - 1] == 7;
            System.out.println("humongous-after-fragmentation " + (ok ? "ok" : "FAILED: kept array corrupted"));
        } catch (OutOfMemoryError e) {
            ok = false;
            System.out.println("humongous-after-fragmentation FAILED: OutOfMemoryError \"" + e.getMessage() + "\"");
        }
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
