// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcd d5/s, reshaped by gcd d6/s and again by gce e1/t (2026-09-29): the
 * in-tree runtime gate for
 * {@code docs/internal/gc/gcd-d2f-shadow-overflow-bail-keeps-moving-gc-FIXED-20260929.md}
 * (JIT round 13's {@code R13CrashIcRecursionSoe} is not in this tree).
 *
 * <p>Two compiled methods recurse into EACH OTHER ({@code diveA} calls
 * {@code diveB} calls {@code diveA}) with THIRTY-TWO young references live
 * across every call, so each level's shadow-stack push is large next to its
 * frame, and the pushes run past the 2 MiB buffer (256K slots) and BAIL well
 * before the recursion stops. Every 1024 levels it allocates 1 MB of garbage,
 * and at the deepest level it allocates 96 MB, so young collections run while
 * every frame -- the bailed ones included -- is live. Each frame checks its
 * objects on the way back up; a young cycle that moved them while a bailed
 * push left them unpublished hands a frame a stale reference ({@code bad>0},
 * or a crash). A shallow phase follows, whose cycles may move again once the
 * shadow stack is back below a quarter full (gcd d3/o).
 *
 * <p>Why two methods (gce e1/t): the d6/s form recursed through ONE method,
 * and a compiled SELF-recursive site is bounded by the JIT's self-call budget
 * (4 MiB below the first observed stack pointer, plus a thread's requested
 * stack growth since JIT round 13). At about 1 KiB of compiled frame per level
 * against 32 pushed words (256 bytes) per level, 4 MiB of frames fill only
 * ~1 MiB of the 2 MiB shadow buffer, so the d6/s runs stopped with
 * {@code StackOverflowError} at ~3 900 levels without a single bail
 * (d8/x: {@code reached=4034 stop=soe}, no overflow line). Since JIT round
 * 13 the compiled stack floor grows by what the carrier was grown for a
 * {@code stackSize} request, so this 1 GiB thread has room for the whole
 * recursion; the mutual form keeps the probe off the self-recursive site's
 * own check, so it exercises the floor every compiled call reads.
 *
 * <p>The recursion stops at {@code CAP} levels or at the first
 * {@link StackOverflowError}, whichever comes first, and neither the depth
 * nor how it stopped is printed on stdout. The reached depth and the stop
 * reason go to stderr.
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx256m -cp tools/bench Gcd1ShadowBailYoungProbe})
 * prints
 * <pre>
 *   shadow-bail deep bad=0
 *   shallow depth=200 frames=201 bad=0
 *   PASS
 * </pre>
 * and exits 0; otherwise the last line is {@code FAIL} and the exit code 1.
 *
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1ShadowBailYoungProbe.java
 *   CRATONVM_MOVING_YOUNG_FALLBACKS=1 timeout 300 cratonvm --java-home "$JDK" \
 *     -XX:+UseGenerationalGC -Xmx256m -cp tools/bench Gcd1ShadowBailYoungProbe
 * </pre>
 * VM-side, on stderr: {@code [probe] deep reached=40000 stop=cap}, and one
 * {@code [JIT] shadow-stack overflow} line. If that line is missing the
 * recursion never bailed and the run proves nothing about the bail (report
 * the reached depth). While the deep phase allocates after the bail, young
 * cycles are fallbacks ({@code reason=shadow-stack-push-bailed}); the shallow
 * phase's cycles are not.
 */
public final class Gcd1ShadowBailYoungProbe {
    /** Upper bound on the deep recursion; HotSpot (1 GiB stack) reaches it. */
    static final int CAP = 40_000;
    static final int WARM = 3_000;

    static final class Obj {
        final int a;
        final int b;
        final int[] pad;

        Obj(int v) {
            this.a = v;
            this.b = v * 3 + 1;
            this.pad = new int[] {v, v + 1};
        }

        int check(int v) {
            return (a == v && b == v * 3 + 1 && pad[0] == v && pad[1] == v + 1) ? 0 : 1;
        }
    }

    static int bad;
    static long sink;
    static boolean bottomDone;
    static int reached;
    static boolean stoppedBySoe;

    /** Garbage enough to trigger young collections. */
    static void churn(int mb) {
        long s = 0;
        for (int i = 0; i < mb * 1024; i++) {
            final int[] g = new int[256];
            g[i & 255] = i;
            s += g[i & 255];
        }
        sink += s;
    }

    /** The deepest level, once: record it and allocate while every frame is live. */
    static void bottom(int depth, int churnMb, boolean soe) {
        if (!bottomDone) {
            bottomDone = true;
            reached = depth;
            stoppedBySoe = soe;
            churn(churnMb);
        }
    }

    /**
     * Thirty-two young references live across the call into {@link #diveB}.
     * Returns the number of frames below and including this one.
     */
    static long diveA(int depth, int cap, int churnMb) {
        final int base = depth * 32;
        final Obj o0 = new Obj(base + 0);
        final Obj o1 = new Obj(base + 1);
        final Obj o2 = new Obj(base + 2);
        final Obj o3 = new Obj(base + 3);
        final Obj o4 = new Obj(base + 4);
        final Obj o5 = new Obj(base + 5);
        final Obj o6 = new Obj(base + 6);
        final Obj o7 = new Obj(base + 7);
        final Obj o8 = new Obj(base + 8);
        final Obj o9 = new Obj(base + 9);
        final Obj o10 = new Obj(base + 10);
        final Obj o11 = new Obj(base + 11);
        final Obj o12 = new Obj(base + 12);
        final Obj o13 = new Obj(base + 13);
        final Obj o14 = new Obj(base + 14);
        final Obj o15 = new Obj(base + 15);
        final Obj o16 = new Obj(base + 16);
        final Obj o17 = new Obj(base + 17);
        final Obj o18 = new Obj(base + 18);
        final Obj o19 = new Obj(base + 19);
        final Obj o20 = new Obj(base + 20);
        final Obj o21 = new Obj(base + 21);
        final Obj o22 = new Obj(base + 22);
        final Obj o23 = new Obj(base + 23);
        final Obj o24 = new Obj(base + 24);
        final Obj o25 = new Obj(base + 25);
        final Obj o26 = new Obj(base + 26);
        final Obj o27 = new Obj(base + 27);
        final Obj o28 = new Obj(base + 28);
        final Obj o29 = new Obj(base + 29);
        final Obj o30 = new Obj(base + 30);
        final Obj o31 = new Obj(base + 31);
        if (churnMb > 0 && (depth & 1023) == 1023) {
            churn(1);
        }
        long below = 0;
        if (depth >= cap) {
            bottom(depth, churnMb, false);
        } else {
            try {
                below = diveB(depth + 1, cap, churnMb);
            } catch (StackOverflowError e) {
                bottom(depth, churnMb, true);
            }
        }
        bad += o0.check(base + 0) + o1.check(base + 1) + o2.check(base + 2)
                + o3.check(base + 3) + o4.check(base + 4) + o5.check(base + 5)
                + o6.check(base + 6) + o7.check(base + 7) + o8.check(base + 8)
                + o9.check(base + 9) + o10.check(base + 10) + o11.check(base + 11)
                + o12.check(base + 12) + o13.check(base + 13) + o14.check(base + 14)
                + o15.check(base + 15) + o16.check(base + 16) + o17.check(base + 17)
                + o18.check(base + 18) + o19.check(base + 19) + o20.check(base + 20)
                + o21.check(base + 21) + o22.check(base + 22) + o23.check(base + 23)
                + o24.check(base + 24) + o25.check(base + 25) + o26.check(base + 26)
                + o27.check(base + 27) + o28.check(base + 28) + o29.check(base + 29)
                + o30.check(base + 30) + o31.check(base + 31);
        return below + 1;
    }

    /** {@link #diveA}'s twin, calling back into it: no self-call site. */
    static long diveB(int depth, int cap, int churnMb) {
        final int base = depth * 32;
        final Obj o0 = new Obj(base + 0);
        final Obj o1 = new Obj(base + 1);
        final Obj o2 = new Obj(base + 2);
        final Obj o3 = new Obj(base + 3);
        final Obj o4 = new Obj(base + 4);
        final Obj o5 = new Obj(base + 5);
        final Obj o6 = new Obj(base + 6);
        final Obj o7 = new Obj(base + 7);
        final Obj o8 = new Obj(base + 8);
        final Obj o9 = new Obj(base + 9);
        final Obj o10 = new Obj(base + 10);
        final Obj o11 = new Obj(base + 11);
        final Obj o12 = new Obj(base + 12);
        final Obj o13 = new Obj(base + 13);
        final Obj o14 = new Obj(base + 14);
        final Obj o15 = new Obj(base + 15);
        final Obj o16 = new Obj(base + 16);
        final Obj o17 = new Obj(base + 17);
        final Obj o18 = new Obj(base + 18);
        final Obj o19 = new Obj(base + 19);
        final Obj o20 = new Obj(base + 20);
        final Obj o21 = new Obj(base + 21);
        final Obj o22 = new Obj(base + 22);
        final Obj o23 = new Obj(base + 23);
        final Obj o24 = new Obj(base + 24);
        final Obj o25 = new Obj(base + 25);
        final Obj o26 = new Obj(base + 26);
        final Obj o27 = new Obj(base + 27);
        final Obj o28 = new Obj(base + 28);
        final Obj o29 = new Obj(base + 29);
        final Obj o30 = new Obj(base + 30);
        final Obj o31 = new Obj(base + 31);
        if (churnMb > 0 && (depth & 1023) == 1023) {
            churn(1);
        }
        long below = 0;
        if (depth >= cap) {
            bottom(depth, churnMb, false);
        } else {
            try {
                below = diveA(depth + 1, cap, churnMb);
            } catch (StackOverflowError e) {
                bottom(depth, churnMb, true);
            }
        }
        bad += o0.check(base + 0) + o1.check(base + 1) + o2.check(base + 2)
                + o3.check(base + 3) + o4.check(base + 4) + o5.check(base + 5)
                + o6.check(base + 6) + o7.check(base + 7) + o8.check(base + 8)
                + o9.check(base + 9) + o10.check(base + 10) + o11.check(base + 11)
                + o12.check(base + 12) + o13.check(base + 13) + o14.check(base + 14)
                + o15.check(base + 15) + o16.check(base + 16) + o17.check(base + 17)
                + o18.check(base + 18) + o19.check(base + 19) + o20.check(base + 20)
                + o21.check(base + 21) + o22.check(base + 22) + o23.check(base + 23)
                + o24.check(base + 24) + o25.check(base + 25) + o26.check(base + 26)
                + o27.check(base + 27) + o28.check(base + 28) + o29.check(base + 29)
                + o30.check(base + 30) + o31.check(base + 31);
        return below + 1;
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < WARM; i++) {
            bottomDone = false;
            sink += diveA(0, 40, 0);
        }
        bad = 0;
        bottomDone = false;
        final long[] frames = new long[1];
        final Thread t = new Thread(null, () -> frames[0] = diveA(0, CAP, 96), "deep", 1L << 30);
        t.start();
        t.join();
        final int deepBad = bad;
        // The frame count equals the reached depth plus one whether the
        // recursion stopped at the cap or at a StackOverflowError.
        final boolean deepConsistent = bottomDone && frames[0] == reached + 1L;
        System.err.println(
                "[probe] deep reached=" + reached + " stop=" + (stoppedBySoe ? "soe" : "cap"));
        System.out.println("shadow-bail deep bad=" + deepBad);
        bad = 0;
        long shallow = 0;
        for (int i = 0; i < 64; i++) {
            bottomDone = false;
            shallow = diveA(0, 200, 2);
        }
        final int shallowBad = bad;
        System.out.println("shallow depth=200 frames=" + shallow + " bad=" + shallowBad);
        final boolean ok = deepBad == 0 && deepConsistent && shallowBad == 0 && shallow == 201;
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
