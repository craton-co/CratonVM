package org.junit.jupiter.api;

import io.netty.handler.codec.http.HttpStatusClass;

/**
 * Compile ORDER, not compile COUNT, decides how fast a call-dense loop runs.
 *
 * Same hot loop, one switch: does warming the callees from a DIFFERENT method
 * BEFORE the hot method is ever compiled change the hot method's speed?
 *
 *   cratonvm --java-home <jdk> -cp <netty test cp> \
 *       org.junit.jupiter.api.CompileOrderProbe 3000000 40 cold
 *   ... 40 prewarm
 *
 * Measured 2026-08-17 (real-JDK mode, G1), interleaved, four runs:
 *
 *   cold     382 / 399 ns/iter   =>  the full HttpResponseStatusTest loop ~1700 s
 *   prewarm   82 /  84 ns/iter   =>  ~355 s
 *
 * 4.6x, deterministic. It is not transient: `BindProbe`-style single-process
 * ordering (time cold, THEN warm the callees, THEN time again) measures 0.95x,
 * i.e. the caller never re-binds once compiled.
 *
 * What it is NOT: the compile records are identical between the two arms (same
 * methods, same tiers, same paths), `CRATONVM_DBG_OSR_BIND=1` reports both of
 * the hot method's own call sites bound DIRECTLY in both arms, and the callee's
 * emitted IR body is the same 703 bytes in both. The mechanism is still open.
 *
 * Lives in `org.junit.jupiter.api` so it can also call the package-private
 * rungs (`AssertEquals`, `AssertionUtils`) the netty tests reach through
 * `Assertions.assertEquals`.
 */
public final class CompileOrderProbe {
    static long sink;
    static final HttpStatusClass UNK = HttpStatusClass.UNKNOWN;

    /** Pre-warms Assertions/AssertEquals/AssertionUtils from a DIFFERENT method. */
    static void prewarm(int n) {
        for (int i = 0; i < n; i++) {
            Assertions.assertEquals(UNK, UNK);
            AssertEquals.assertEquals(UNK, UNK);
            if (!AssertionUtils.objectsAreEqual(UNK, UNK)) { throw new IllegalStateException(); }
        }
    }

    static void hot(int from, int n) {
        for (int i = from; i < from + n; i++) {
            Assertions.assertEquals(UNK, HttpStatusClass.valueOf(i));
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 8_000_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        boolean warm = args.length > 2 && args[2].equals("prewarm");
        if (warm) { prewarm(3_000_000); }
        int per = n / reps;
        for (int w = 0; w < reps; w++) { hot(600, per); }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) { hot(600, per); }
        long t1 = System.nanoTime();
        double ns = (double) (t1 - t0) / (per * (long) reps);
        System.out.printf("%-9s %8.2f ns/iter => full = %.1f s%n", warm ? "prewarm" : "cold", ns, ns * 4294967296.0 / 1e9);
    }
}
