// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Stack-trace fidelity across the tiering boundary.
 *
 * Throws from ONE site three times in one process; only the amount of prior
 * warm-up differs. A correct VM prints the same five frames every time, with
 * only `main`'s line moving. Run against HotSpot with
 * `-XX:-OmitStackTraceInFastThrow` (HotSpot otherwise drops the trace of a
 * repeated implicit NPE altogether, which is a different behaviour, not this
 * one).
 *
 * Compile with `-g` so the LineNumberTable is present.
 *
 *   java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceAfterOsr
 *   cratonvm -cp probes StackTraceAfterOsr
 *   CRATONVM_DISABLE_JIT=1 cratonvm -cp probes StackTraceAfterOsr
 *
 * See jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md
 */
public class StackTraceAfterOsr {
    static int[][] table = new int[8][];

    /**
     * The index expression of the throw site, and the one thing in this probe
     * that is there for the COMPILER rather than for the trace.
     *
     * `MARK.length()` is 0, so it changes nothing a reader of the trace can
     * see. What it changes is whether `leaf` can be SPLICED into its callers:
     * the inline resolver refuses a callee that calls something it can neither
     * splice nor direct-bind, and `java/lang/String.length()I` is such a call
     * ("inline-resolve REFUSED ... is neither spliced nor direct-bound").
     *
     * That refusal is what gives the third row its shape. With it, `probe`'s
     * compiled body splices `outer` and `mid` and CALLS `leaf`, so the hot
     * throw stands in an artifact with inlined callees and
     * `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` has two frames to take away —
     * which is the only thing that makes that switch's arm in
     * vm/tests/stack_trace_across_tiers.rs measure anything.
     *
     * Without it — the shape this probe had until 2026-09-11 — `leaf` is
     * spliced too, all the way up. `probe`'s compiled body then contains the
     * throw itself, traps on it (`UnreachedCode` at the splice, then
     * `MakeNotCompilable`) and is reinterpreted the first time it is entered;
     * so are `outer` and `mid`. Every frame below the OSR'd `main` is an
     * interpreter frame at the third throw, the trace is still correct and
     * still matches the interpreter, and the switch has nothing to revert.
     * That is a silent loss of the anti-vacuity half of this probe, and it is
     * how it was lost once already.
     */
    static final String MARK = "";

    static int leaf(int i)  { return table[i & 7][MARK.length()]; }   // the throw site
    static int mid(int i)   { return leaf(i) + 1; }
    static int outer(int i) { return mid(i) + 1; }

    static String tr(Throwable e) {
        StackTraceElement[] st = e.getStackTrace();
        StringBuilder sb = new StringBuilder("len=").append(st.length).append(" [");
        for (StackTraceElement s : st) {
            sb.append(s.getClassName()).append('.').append(s.getMethodName())
              .append(':').append(s.getLineNumber()).append(' ');
        }
        return sb.append(']').toString();
    }

    /** The throw, wrapped so the trace always has the same shape. */
    static String probe() {
        table[1] = null;
        try { outer(1); return "no-ex"; }
        catch (NullPointerException e) { return tr(e); }
        finally { table[1] = new int[]{1}; }
    }

    /** Warms leaf/mid/outer from a HELPER, so main() itself stays interpreted. */
    static long warmHere(int n) { long a = 0; for (int r = 0; r < n; r++) a += outer(r); return a; }

    public static void main(String[] args) {
        for (int i = 0; i < 8; i++) table[i] = new int[]{i};

        // (a) everything interpreted
        System.out.println("before_any_warm=" + probe());

        // (b) leaf/mid/outer compiled and inlined, main still interpreted
        System.out.println("warm=" + (warmHere(400_000) != 0));
        System.out.println("after_helper_warm=" + probe());

        // (c) main now has a hot loop of its own, so it OSR-compiles
        long a = 0;
        for (int r = 0; r < 400_000; r++) a += r ^ (r >>> 3);
        System.out.println("main_looped=" + (a != 0));

        // Same throw site as (a) and (b). This is the one that degrades.
        System.out.println("after_main_osr=" + probe());
    }
}
