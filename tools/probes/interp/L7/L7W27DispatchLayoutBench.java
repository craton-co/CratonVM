// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 27, lane L7: the dispatch loop's code generation
// under fat LTO
// (docs/known-issues/interpreter/i25-L7-the-interpreter-dispatch-loop-is-slower-than-on-wave-23-20260927.md,
// "Progress (wave 27)").
//
// Wave 27 changed no behaviour; it changed what execute_frame_from_index and
// the functions its arms call look like to the compiler. Each row isolates
// one arm family a commit touched, so an A/B can say which commit moved what
// (TypeCheckBench, InvokeDoorCostBench, L7W24ArrayElementBench,
// L4W25FramelessDoorBench and BackEdgeCounterBench are the page's own rows;
// run them too):
//
//   ldcBound    `i < 1_000_000`: an `ldc` of an int every iteration (commit
//               "split execute_ldc": the arm's call now reaches a small
//               probe function). Expected: down.
//   refLocals   aload / astore of reference locals (six aloads, four
//               astores and an if_acmpne per iteration; commit "out-of-line
//               cold halves for the reference
//               load/store helpers"). Expected: flat or down.
//   intReturn   a static call returning an int (commit "stop materializing
//               the returned Value"). Expected: down a little.
//   refReturn   a static call returning a reference (areturn keeps its
//               Value). Expected: flat.
//   castCall    `if (o instanceof B) s += ((B) o).v()` -- TypeCheckBench's
//               classMono shape (commits "own fast-path arms" and "split
//               checkcast/instanceof"). Expected: down.
//   castOnly    `if (o instanceof B) s += ((B) o).f` (a field, no call):
//               separates the type checks from the call. Expected: down.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W27DispatchLayoutBench
//      on fat-LTO builds (the shipped profile), interleaved with the base
//      build (faa212874), pinned to one core, 5 rounds, medians.
// stdout is deterministic, identical on HotSpot 25:
//   ldcBound checksum=1500000
//   refLocals checksum=500000
//   intReturn checksum=-728379968
//   refReturn checksum=1000000
//   castCall checksum=1000000
//   castOnly checksum=7000000
// stderr: ns per iteration per row (min of REPS).
public class L7W27DispatchLayoutBench {
    static class Base {
        int f = 7;

        int v() {
            return 1;
        }
    }

    static final class Sub extends Base {}

    static final int N = 1_000_000;
    static final int REPS = 5;
    static final Object TOKEN = new Object();

    static int ldcBound() {
        int s = 0;
        for (int i = 0; i < 1_000_000; i++) {
            s += i & 3;
        }
        return s;
    }

    static int refLocals(Object x, Object y) {
        Object a = x;
        Object b = y;
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object t = a;
            a = b;
            b = t;
            Object u = a;
            if (u == x) {
                s++;
            }
        }
        return s;
    }

    static int twice(int i) {
        return i + i;
    }

    static int intReturn() {
        int s = 0;
        for (int i = 0; i < N; i++) {
            s += twice(i);
        }
        return s;
    }

    static Object same(Object o) {
        return o;
    }

    static int refReturn() {
        int s = 0;
        for (int i = 0; i < N; i++) {
            if (same(TOKEN) == TOKEN) {
                s++;
            }
        }
        return s;
    }

    static int castCall(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i & 1];
            if (o instanceof Base) {
                s += ((Base) o).v();
            }
        }
        return s;
    }

    static int castOnly(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i & 1];
            if (o instanceof Base) {
                s += ((Base) o).f;
            }
        }
        return s;
    }

    interface Row {
        int run();
    }

    static void row(String name, Row r) {
        int checksum = 0;
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            int c = r.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (rep == 0) {
                checksum = c;
            } else if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.err.printf("%-10s %8.2f ns/iter (min of %d)%n", name, (double) best / N, REPS);
    }

    public static void main(String[] args) {
        Object[] subs = {new Sub(), new Sub()};
        Object p = new Object();
        Object q = new Object();
        row("ldcBound", L7W27DispatchLayoutBench::ldcBound);
        row("refLocals", () -> refLocals(p, q));
        row("intReturn", L7W27DispatchLayoutBench::intReturn);
        row("refReturn", L7W27DispatchLayoutBench::refReturn);
        row("castCall", () -> castCall(subs));
        row("castOnly", () -> castOnly(subs));
    }
}
