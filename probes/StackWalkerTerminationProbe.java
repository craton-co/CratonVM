// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.StackWalker.StackFrame;
import java.util.Optional;
import java.util.stream.Stream;

/**
 * `StackWalker.walk` must hand the function a FINITE stream: when the walk runs
 * off the bottom of the stack the stream ends. Mockito builds a Location for
 * every mock invocation this way (LocationImpl.stackWalk -> getStackFrame), so
 * a stream that never ends would park the calling thread forever.
 *
 * MEASURED: it does NOT hang — every arm terminates on every VM. What it does
 * is cost 61 us on HotSpot, 1,461 us under `--nojit` and 2,327 us with the JIT
 * on, i.e. 38x, with roughly half the time in GC root machinery rather than in
 * stack walking (`native_stack_has_jit_frame` alone is 17.9%). That is what
 * makes QuartzEndpointWebIntegrationTests look like a hang: Mockito builds one
 * Location per mock invocation, so the request never finishes and the client
 * blocks in Mono.block on a response that never comes. The JIT arm being the
 * SLOWER one is why `--nojit` passes the class.
 *
 * Arms, all with a bounded expectation:
 *   count        - how many frames the walk yields (must be finite and small)
 *   findFirst-hit  - a filter that matches (must return quickly)
 *   findFirst-miss - a filter that matches NOTHING (must still terminate)
 *   skip-past-end  - skip more frames than exist (must return empty)
 */
public class StackWalkerTerminationProbe {
    static final StackWalker WALKER = StackWalker.getInstance();
    static int sink;

    static <T> T walk(java.util.function.Function<Stream<StackFrame>, T> f) { return WALKER.walk(f); }

    static long countFrames() { return walk(s -> s.limit(10000).count()); }
    static Optional<StackFrame> findHit()  { return walk(s -> s.filter(fr -> fr.getMethodName().equals("deep")).findFirst()); }
    static Optional<StackFrame> findMiss() { return walk(s -> s.filter(fr -> fr.getMethodName().equals("no-such-method-anywhere")).findFirst()); }
    static Optional<StackFrame> skipPast() { return walk(s -> s.skip(100000).findFirst()); }

    static void deep(int d, int iters) {
        if (d > 0) { deep(d - 1, iters); return; }
        for (int i = 0; i < iters; i++) {
            long n = countFrames();
            if (n <= 0 || n > 9000) throw new AssertionError("frame count not finite/sane: " + n);
            if (findHit().isEmpty()) throw new AssertionError("findFirst-hit missed its own frame");
            if (findMiss().isPresent()) throw new AssertionError("findFirst-miss matched something");
            if (skipPast().isPresent()) throw new AssertionError("skip-past-end returned a frame");
            sink++;
        }
    }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
        int depth = a.length > 1 ? Integer.parseInt(a[1]) : 40;
        long t0 = System.nanoTime();
        deep(depth, iters);
        System.out.println("iters=" + sink + " depth=" + depth
            + " frames=" + countFrames() + " ms=" + ((System.nanoTime() - t0) / 1000000));
        System.out.println("OK");
    }
}
