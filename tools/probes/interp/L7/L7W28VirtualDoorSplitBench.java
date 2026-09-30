// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 28, lane L7: the virtual/interface fast door split
// into a hit path and out-of-line tails
// (docs/known-issues/interpreter/i27-L7-proposal-split-the-virtual-fast-door-into-a-hit-and-out-of-line-tails-20260928.md).
//
// Two commits, measured separately:
//   A  "the virtual fast door's declines, tier-up and compiled call out of
//      line" (cold decline helpers, `virtual_door_tier_up`,
//      `virtual_door_call_compiled`): the door's stack frame loses a
//      256-byte argument buffer and ~17 inlined decline tails.
//   B  "the virtual fast door is never inlined into the dispatch loop"
//      (`#[inline(never)]` on `execute_invokevirtual_fast_door`).
//
// Rows (every call in the timed loop goes through the virtual door):
//
//   virtualMono   a monomorphic invokevirtual of an interpreted callee with
//                 a body (the door's verbatim frame push). Expected: A down
//                 a few ns; B flat.
//   ifaceMono     the same through invokeinterface (the interface
//                 selection memo hit). Expected: as virtualMono.
//   virtualPoly   a bimorphic site (the poly entry). Expected: as
//                 virtualMono.
//   syncMono      a synchronized callee, uncontended (the door's monitor
//                 acquire). Expected: as virtualMono.
//   getterMono    a trivial getter (answered without a frame). Expected:
//                 flat or down.
//   emptyMono     an empty body (answered without a frame). Expected: flat
//                 or down.
//   staticCall    CONTROL: an invokestatic of the same body (not the
//                 virtual door). Expected: flat.
//
// Run, fat-LTO builds (the shipped profile), interleaved with the parent of
// commit A (and A against B), pinned to one core, 5 rounds, medians:
//
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W28VirtualDoorSplitBench
//
// and once with the JIT on (no `--nojit`): the tier-up block and the compiled
// call now sit behind one call each, so the JIT-on rows must not move beyond
// the floor (they compile early and mostly measure compiled code).
//
// stdout is deterministic, identical on HotSpot 25 (25.0.3), with and without
// `--nojit`:
//   virtualMono checksum=1000000000000
//   ifaceMono checksum=1000000000000
//   virtualPoly checksum=1000000500000
//   syncMono checksum=1000000000000
//   getterMono checksum=3000000
//   emptyMono checksum=3500000
//   staticCall checksum=1000000000000
// stderr: ns per call per row (min of REPS). For scale, HotSpot 25 -Xint on
// the i7-8550U box: virtualMono 69, ifaceMono 77, virtualPoly 84, syncMono 121,
// getterMono 89, emptyMono 138, staticCall 116 ns/call.
public class L7W28VirtualDoorSplitBench {
    interface Twice {
        int twice(int x);
    }

    static class Box implements Twice {
        int v = 3;

        public int twice(int x) {
            return x + x + 1;
        }

        synchronized int syncTwice(int x) {
            return x + x + 1;
        }

        int get() {
            return v;
        }

        void hook(int x) {
        }
    }

    static final class OtherBox extends Box {
        @Override
        public int twice(int x) {
            return x + x + 2;
        }
    }

    static final int N = 1_000_000;
    static final int REPS = 5;

    static int staticTwice(int x) {
        return x + x + 1;
    }

    static long virtualMono(Box b) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += b.twice(i);
        }
        return s;
    }

    static long ifaceMono(Twice t) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += t.twice(i);
        }
        return s;
    }

    static long virtualPoly(Box a, Box b) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += ((i & 1) == 0 ? a : b).twice(i);
        }
        return s;
    }

    static long syncMono(Box b) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += b.syncTwice(i);
        }
        return s;
    }

    static long getterMono(Box b) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += b.get();
        }
        return s;
    }

    static long emptyMono(Box b) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            b.hook(i);
            s += i & 7;
        }
        return s;
    }

    static long staticCall() {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += staticTwice(i);
        }
        return s;
    }

    interface Row {
        long run();
    }

    static void row(String name, Row r) {
        long sum = 0;
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            sum = r.run();
            long ns = System.nanoTime() - t0;
            best = Math.min(best, ns);
        }
        System.out.println(name + " checksum=" + sum);
        System.err.printf(java.util.Locale.ROOT, "%-12s %8.1f ns/call%n", name, (double) best / N);
    }

    public static void main(String[] args) {
        Box box = new Box();
        Box other = new OtherBox();
        row("virtualMono", () -> virtualMono(box));
        row("ifaceMono", () -> ifaceMono(box));
        row("virtualPoly", () -> virtualPoly(box, other));
        row("syncMono", () -> syncMono(box));
        row("getterMono", () -> getterMono(box));
        row("emptyMono", () -> emptyMono(box));
        row("staticCall", L7W28VirtualDoorSplitBench::staticCall);
    }
}
