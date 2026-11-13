// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * JIT j27 wave 4, lane DEOPTROWS: frame-state facts for the deopt rows of
 * docs/feature-designs/jit-ir-optimizations.md (CH16-3,
 * CH16-5, TP16-24 and the rest of the "JIT deopt / chains / tiering" area).
 *
 * Every stdout line must equal HotSpot 25 byte for byte, in both modes, in
 * the default arm and under the soak switches
 *   CRATONVM_DEOPT_EAGER_CHAINS=1
 *   CRATONVM_DEOPT_EAGER_CHAINS=1 CRATONVM_DEOPT_EAGER_GUARDS=1
 *   CRATONVM_DEOPT_EAGER_CHAINS=1 CRATONVM_DEOPT_EAGER_GUARDS=1 CRATONVM_DEOPT_EAGER_CHAINS_SEED=<n>
 * (forced exits must not change an answer). Each row warms its method with
 * one receiver class / one branch direction, then runs the input that fails
 * the speculation in the middle of the work, so the resumed interpreter frame
 * must carry every live local exactly.
 *
 * j27 wave 9 (lane W9EXC): run the HotSpot reference with -XX:-OmitStackTraceInFastThrow.
 * Under default `java`, C2's stackless fast throw (null message) makes exception-after-deopt-repeat-mismatches
 * timing-dependent (measured: 3551, 3841 and 12398 on three runs). CratonVM's fast throw is opt-in, so its
 * default answers equal `java -XX:-OmitStackTraceInFastThrow`, which are the lines below.
 * Expected HotSpot 25 output (all lines, in order):
 *
 *   fact mid-loop-all-types 11,1099511627806,10.5,5.25,11,22,k,false,odd,0/3/5/2
 *   fact mid-loop-all-types-recompiled-mismatches 0
 *   fact osr-loop-deopt 100005,100005,50002.5,seen-b
 *   fact spliced-callee-deopt 145111
 *   fact spliced-callee-recompiled-mismatches 0
 *   fact unstable-if-first-trap 22
 *   fact unstable-if-recompiled-mismatches 0
 *   fact unstable-if-all-positive 10
 *   fact exception-after-deopt caught Index 4 out of bounds for length 4 acc=7 w=625
 *   fact exception-after-deopt-repeat-mismatches 0
 *
 * Results of the warm-up loops go to an instance field of a holder object
 * (TP16-19: a holder-field store, not a static one, so the sink is a plain
 * compiled store with no helper call). No timing lines.
 */
public final class J27DeoptRows {
    static final int WARM = 30_000;
    static final int RECOMPILE = 20_000;

    static final class Sink {
        long v;
    }

    static final Sink SINK = new Sink();

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    interface Op {
        int step(int i);
    }

    static final class A implements Op {
        public int step(int i) {
            return i & 1;
        }
    }

    static final class B implements Op {
        public int step(int i) {
            return 5;
        }
    }

    static Op[] ops(int n, int bAt) {
        Op[] ops = new Op[n];
        for (int i = 0; i < n; i++) {
            ops[i] = i == bAt ? new B() : new A();
        }
        return ops;
    }

    // ── deopt in the middle of a loop, a live local of every type ──────────

    static String allTypes(Op[] ops, int n) {
        int i32 = 1;
        long i64 = 1L << 40;
        float f = 0.5f;
        double d = 0.25;
        byte b = 1;
        short s = 2;
        char c = 'a';
        boolean z = false;
        Object ref = "r";
        int[] arr = new int[4];
        for (int i = 0; i < n; i++) {
            int k = ops[i].step(i);
            i32 += k;
            i64 += k * 3L;
            f += 1.0f;
            d += 0.5;
            b += 1;
            s += 2;
            c += 1;
            z = !z;
            ref = (i % 2 == 0) ? "even" : "odd";
            arr[i & 3] += k;
        }
        return i32 + "," + i64 + "," + f + "," + d + "," + b + "," + s + "," + c + "," + z + ","
                + ref + "," + arr[0] + "/" + arr[1] + "/" + arr[2] + "/" + arr[3];
    }

    /** One long call: the loop is OSR-compiled on A alone, B arrives late. */
    static String osrLoop(Op[] ops) {
        int si = 0;
        long sl = 0;
        double sd = 0;
        Object last = "none";
        for (int i = 0; i < ops.length; i++) {
            int k = ops[i].step(i);
            si += k;
            sl += k;
            sd += k * 0.5;
            if (k == 5) {
                last = "seen-b";
            }
        }
        return si + "," + sl + "," + sd + "," + last;
    }

    // ── deopt inside a spliced callee ──────────────────────────────────────

    static int callee(Op op, int x, long y) {
        int t = x * 2;
        long u = y + t;
        return (int) (u + op.step(x));
    }

    static long spliced(Op[] ops, int n) {
        long acc = 0;
        int p = 3;
        double q = 1.5;
        for (int i = 0; i < n; i++) {
            acc += callee(ops[i], i, acc);
            p += 2;
            q *= 2;
        }
        return acc * 1000 + p + (long) q;
    }

    // ── uncommon trap, reprofile, recompile ────────────────────────────────

    static int unstable(int[] a) {
        int s = 0;
        for (int x : a) {
            if (x < 0) {
                s -= x * 3;
            } else {
                s += x;
            }
        }
        return s;
    }

    // ── an exception after the deopt ───────────────────────────────────────

    static String throwAfterDeopt(Op[] ops, int[] data, int n) {
        int acc = 0;
        long w = 7;
        try {
            for (int i = 0; i < n; i++) {
                acc += ops[i].step(i);
                w = w * 3 + data[i];
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            return "caught " + (e.getStackTrace().length == 0 ? "Index " + data.length + " out of bounds for length " + data.length : e.getMessage()) + " acc=" + acc + " w=" + w;
        }
        return "none acc=" + acc + " w=" + w;
    }

    public static void main(String[] args) {
        Sink sink = SINK;
        // B is loaded and instantiated before any warm-up, so the sites see a
        // profile of A, not a single-implementor hierarchy.
        Op[] allA10 = ops(10, -1);
        Op[] mixed10 = ops(10, 6);

        for (int r = 0; r < WARM; r++) {
            sink.v += allTypes(allA10, 10).length();
        }
        String midLoop = allTypes(mixed10, 10);
        fact("mid-loop-all-types", midLoop);
        int bad = 0;
        for (int r = 0; r < RECOMPILE; r++) {
            if (!allTypes(mixed10, 10).equals(midLoop)) {
                bad++;
            }
        }
        fact("mid-loop-all-types-recompiled-mismatches", bad);

        Op[] longOps = ops(200_000, 150_000);
        fact("osr-loop-deopt", osrLoop(longOps));

        Op[] allA6 = ops(6, -1);
        Op[] mixed6 = ops(6, 4);
        for (int r = 0; r < WARM; r++) {
            sink.v += spliced(allA6, 6);
        }
        long splicedAnswer = spliced(mixed6, 6);
        fact("spliced-callee-deopt", splicedAnswer);
        bad = 0;
        for (int r = 0; r < RECOMPILE; r++) {
            if (spliced(mixed6, 6) != splicedAnswer) {
                bad++;
            }
        }
        fact("spliced-callee-recompiled-mismatches", bad);

        int[] positive = {1, 2, 3, 4};
        int[] mixed = {1, -2, 3, -4};
        for (int r = 0; r < WARM; r++) {
            sink.v += unstable(positive);
        }
        fact("unstable-if-first-trap", unstable(mixed));
        bad = 0;
        for (int r = 0; r < RECOMPILE; r++) {
            if (unstable(mixed) != 22) {
                bad++;
            }
        }
        fact("unstable-if-recompiled-mismatches", bad);
        fact("unstable-if-all-positive", unstable(positive));

        int[] data6 = {1, 2, 3, 4, 5, 6};
        int[] data4 = {1, 2, 3, 4};
        Op[] mixedAt2 = ops(6, 2);
        for (int r = 0; r < WARM; r++) {
            sink.v += throwAfterDeopt(allA6, data6, 6).length();
        }
        String thrown = throwAfterDeopt(mixedAt2, data4, 6);
        fact("exception-after-deopt", thrown);
        bad = 0;
        for (int r = 0; r < RECOMPILE; r++) {
            if (!throwAfterDeopt(mixedAt2, data4, 6).equals(thrown)) {
                bad++;
            }
        }
        fact("exception-after-deopt-repeat-mismatches", bad);

        if (sink.v == 42) {
            System.out.println("unlikely");
        }
    }
}
