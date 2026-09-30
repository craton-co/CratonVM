// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcd d4/n (2026-09-28): the probe
 * {@code docs/internal/gc/gcd-d3m-phase5-compacts-old-gen-under-unmapped-compiled-words-FIXED-20260928.md}
 * asks for. A compiled loop walks an OLD array (a derived cursor into it,
 * which no oop map names) while another thread's humongous request needs the
 * old generation compacted. If the collection slides the array under the
 * loop, the loop reads the zeroed or reused bytes behind its stale cursor and
 * its checksum changes (or the process faults).
 *
 * <p>Geometry at {@code -Xmx128m} (as {@code GenR4W4HumongousFragProbe}): 32 MiB
 * semi-spaces, a 64 MiB old generation, humongous threshold 16 MiB. Three
 * 17 MiB arrays go to old gen back to back; the first and third are dropped and
 * the middle one ({@code keep}) is the array the cursor thread walks. A 33 MiB
 * request fits no hole until {@code keep} slides down.
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx128m}) prints on stdout, every run:
 * <pre>
 *   cursor-checksum ok
 *   PASS
 * </pre>
 * {@code PASS} is the cursor verdict only (no corruption).
 *
 * <p>gcd d5/r (2026-09-28): the humongous outcome moved to STDERR
 * ({@code info (not HotSpot-comparable): humongous-after-fragmentation ...}),
 * because it measures heap geometry, not the hazard. Measured with
 * {@code -Xlog:gc+heap=debug}: Serial at {@code -Xmx128m} has an 87424K tenured
 * generation and a 34944K eden, so the 33 MiB array fits EDEN and HotSpot runs
 * no full collection at all (two young pauses, no compaction). CratonVM's
 * default geometry at {@code -Xmx128m} is two 32 MiB semi-spaces and a 64 MiB
 * old generation with a 16 MiB humongous threshold, so the request needs
 * {@code keep} slid down, and {@code keep} is exactly what the compiled cursor
 * names by a derived word: no collector may move it. By default the request
 * is answered by the allocation ladder's REQUESTED major, which takes the
 * NON-MOVING cycle ({@code explicit_full_gc}); that path's default humongous
 * compaction declines while any conservative root is published (the cursor's
 * compiled frame), so the line reads {@code FAILED: OutOfMemoryError} in BOTH
 * arms of {@code CRATONVM_GC_MOVING_MAJOR_JIT_GUARD}, and the guard is never
 * consulted. To reach the moving path's Phase 5 (the hazard), add
 * {@code CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1} (the requested major's young half
 * may then copy). A run counts only when {@code CRATONVM_DBG=gc-stats} shows
 * {@code oldsz_moving_major_vetoes>=1} (guard arm) or {@code oldsz_compactions>=1}
 * ({@code =0} arm); the {@code =0} arm then shows the hazard as a
 * {@code cursor-checksum FAILED} line or a fault.
 * Ends on its own: a 120 s internal deadline, then {@code FAIL deadline}.
 * <pre>
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench Gcd4OldCursorCompactProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench Gcd4OldCursorCompactProbe
 *   CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench Gcd4OldCursorCompactProbe
 *   CRATONVM_GC_MOVING_MAJOR_JIT_GUARD=0 CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench Gcd4OldCursorCompactProbe
 * </pre>
 */
public final class Gcd4OldCursorCompactProbe {
    static final int MIB = 1 << 20;
    static final long DEADLINE_NANOS = 120L * 1_000_000_000L;
    static volatile Object sink;
    static byte[] keep;
    static volatile boolean stop;
    static volatile long passes;
    static volatile long bad;

    /** Allocates A, B, C back to back; keeps only B. Own frame: no dead local roots A or C. */
    static void fragment() {
        byte[] a = new byte[17 * MIB];
        byte[] b = new byte[17 * MIB];
        byte[] c = new byte[17 * MIB];
        a[0] = 1;
        c[0] = 1;
        for (int i = 0; i < b.length; i++) {
            b[i] = (byte) (i * 31 + 7);
        }
        sink = a;
        sink = c;
        sink = null;
        keep = b;
    }

    /** The cursor loop: compiled after a few passes; one derived pointer per pass. */
    static long sum(byte[] data) {
        long s = 0;
        for (int i = 0; i < data.length; i++) {
            s += data[i];
        }
        return s;
    }

    static byte[] allocate(int bytes) {
        byte[] d = new byte[bytes];
        d[bytes - 1] = 1;
        return d;
    }

    public static void main(String[] args) throws Exception {
        long start = System.nanoTime();
        fragment();
        final byte[] walked = keep;
        final long expected = sum(walked);
        Thread cursor = new Thread(() -> {
            while (!stop) {
                if (sum(walked) != expected) {
                    bad++;
                }
                passes++;
            }
        }, "cursor");
        cursor.setDaemon(true);
        cursor.start();
        // Warm the loop (compiled, and running when the request comes).
        while (passes < 40 && System.nanoTime() - start < DEADLINE_NANOS / 2) {
            Thread.sleep(5);
        }
        String humongous;
        try {
            byte[] d = allocate(33 * MIB);
            sink = d;
            humongous = "humongous-after-fragmentation ok";
        } catch (OutOfMemoryError e) {
            humongous = "humongous-after-fragmentation FAILED: OutOfMemoryError \"" + e.getMessage() + "\"";
        }
        sink = null;
        // A few more passes over whatever the collection left behind.
        long after = passes + 5;
        while (passes < after && System.nanoTime() - start < DEADLINE_NANOS) {
            Thread.sleep(5);
        }
        stop = true;
        cursor.join(DEADLINE_NANOS / 1_000_000L);
        if (cursor.isAlive() || System.nanoTime() - start >= DEADLINE_NANOS) {
            System.out.println("FAIL deadline");
            System.exit(2);
        }
        boolean ok = bad == 0 && sum(walked) == expected;
        System.out.println(ok ? "cursor-checksum ok" : "cursor-checksum FAILED bad=" + bad);
        // Geometry, not the verdict (see the class comment): stderr only.
        System.err.println("info (not HotSpot-comparable): " + humongous);
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
