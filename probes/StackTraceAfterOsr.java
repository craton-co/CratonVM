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
 * See docs/known-issues/jit-compiled-frame-has-no-line-and-no-inlined-callees-20260901.md
 */
public class StackTraceAfterOsr {
    static int[][] table = new int[8][];

    static int leaf(int i)  { return table[i & 7][0]; }   // the throw site
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
