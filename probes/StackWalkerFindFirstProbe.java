// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.StackWalker.StackFrame;
import java.util.Optional;

/**
 * The ONE stream shape `quartz-stackwalker-walk-is-38x-hotspot` is about:
 * Mockito's `LocationImpl` builds a `Location` per mock invocation with
 * `walk(s -> s.filter(..).findFirst())`, and it reads `getClassName()` on each
 * frame the filter visits.
 *
 * `StackWalkerTerminationProbe` cannot settle whether a BATCHED walk helps,
 * because three of its four arms (`count`, `findFirst`-miss, `skip`-past-end)
 * traverse the whole stack by construction — batching can only add round trips
 * to those. This probe separates the two ends of the range explicitly:
 *
 *   near — the predicate matches the walk()-CALLER, so a lazy walk inspects
 *          ONE frame and an eager one still materialises all of them. This is
 *          the shape a batched implementation is supposed to win.
 *   far  — the predicate matches the OUTERMOST frame, so every frame is
 *          inspected either way. This is the control: it says what the walk
 *          costs when laziness cannot help.
 *
 * Both arms assert they found their frame, so an arm that is fast because it
 * matched nothing fails instead of posting a number.
 */
public class StackWalkerFindFirstProbe {
    static final StackWalker WALKER = StackWalker.getInstance();
    static int sink;

    static Optional<StackFrame> near() {
        return WALKER.walk(s -> s.filter(f -> f.getClassName()
                .equals("StackWalkerFindFirstProbe")
                && f.getMethodName().equals("near")).findFirst());
    }

    static Optional<StackFrame> far() {
        return WALKER.walk(s -> s.filter(f -> f.getMethodName().equals("main")).findFirst());
    }

    static void deep(int d, int iters, boolean isNear) {
        if (d > 0) {
            deep(d - 1, iters, isNear);
            return;
        }
        for (int i = 0; i < iters; i++) {
            Optional<StackFrame> hit = isNear ? near() : far();
            if (hit.isEmpty()) {
                throw new AssertionError("predicate matched nothing — the arm is vacuous");
            }
            sink++;
        }
    }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 2000;
        int depth = a.length > 1 ? Integer.parseInt(a[1]) : 40;
        String arm = a.length > 2 ? a[2] : "near";
        boolean isNear = arm.equals("near");
        // Warm up so both VMs measure steady state.
        deep(depth, Math.min(iters, 200), isNear);
        sink = 0;
        long t0 = System.nanoTime();
        deep(depth, iters, isNear);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("arm=" + arm + " iters=" + sink + " depth=" + depth + " ms=" + ms);
        System.out.println("OK");
    }
}
