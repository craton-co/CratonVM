// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gce e1/f (2026-09-29): items 2 and 3 of
// docs/known-issues/gc/gengc-r4w5-oomjit5-exceptional-frame-orphans-outlive-a-compiled-catch-20260924.md
// asked for this probe: an exception thrown through an OSR'd caller by a
// compiled callee whose own handler does NOT match.
//
// `callee` throws an IllegalStateException from inside its `try`, whose only
// handler catches ArithmeticException and reads `big`. A compiled `callee`
// therefore publishes its precise exceptional frame (reason 9) on the miss
// edge, and that frame names `big` (live at the handler). The exception
// leaves `callee`, then the OSR'd `loop`, and is caught in `main`. From then
// on nothing reads the frame: `big` (16 MiB) is garbage, and a weak reference
// to it must clear at the next collections. Item 2 is the OSR sink
// (`route_osr_exception_out_of_artifact`) re-stashing that foreign frame as
// the exception leaves the OSR body, which keeps `big` reachable through the
// deopt stash (census label `vm/deopt-stash`).
//
//   javac -d tools/probes tools/probes/GceE1fOsrOrphanExceptionalFrameProbe.java
//   java|cratonvm [-XX:+UseG1GC|-XX:+UseZGC] -Xmx64m -cp tools/probes GceE1fOsrOrphanExceptionalFrameProbe
//
// Expected (HotSpot 25 -XX:+UseSerialGC, and -Xint):
//   caught=true
//   PASS osr-orphan-frame
// This VM with the orphan kept: `FAIL osr-orphan-frame` and rc 1. A/B arm:
// CRATONVM_JIT_OSR_DROP_ORPHANS=1 (the opt-in drop) should print PASS. The
// census (CRATONVM_DBG=oldmark-root-census) names the holder `vm/deopt-stash`
// when it fails. CRATONVM_DBG_JITC=1 should show an OSR compile of
// `GceE1fOsrOrphanExceptionalFrameProbe.loop`; without it the run did not
// reach the shape (raise the first argument, the iteration count).
import java.lang.ref.WeakReference;

public class GceE1fOsrOrphanExceptionalFrameProbe {
    static final int BIG = 16 << 20;
    static final byte[] SMALL = new byte[3];
    static WeakReference<byte[]> ref;

    static int callee(int i, int trigger) {
        byte[] big = i == trigger ? new byte[BIG] : SMALL;
        if (i == trigger) {
            ref = new WeakReference<>(big);
        }
        try {
            if (i == trigger) {
                throw new IllegalStateException("escapes callee");
            }
            return big.length & 1;
        } catch (ArithmeticException e) {
            return big.length;
        }
    }

    static long loop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += callee(i, n - 1);
        }
        return s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 3_000_000;
        boolean caught = false;
        try {
            loop(n);
        } catch (IllegalStateException e) {
            caught = true;
        }
        System.out.println("caught=" + caught);
        boolean cleared = false;
        for (int round = 0; round < 5 && !cleared; round++) {
            System.gc();
            cleared = ref != null && ref.get() == null;
        }
        System.out.println((cleared ? "PASS" : "FAIL") + " osr-orphan-frame");
        if (!cleared || !caught) {
            System.exit(1);
        }
    }
}
