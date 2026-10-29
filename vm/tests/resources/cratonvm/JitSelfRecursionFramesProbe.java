// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

package cratonvm;

/**
 * Regression probe for the JIT nested-activation stack walk
 * (docs/known-issues/jit/jit-self-recursive-activations-invisible-to-stack-walks-20260818.md).
 *
 * A DIRECTLY self-recursive method's activations used to collapse to a single
 * frame once the method tiered up, because `JIT_ENTRY_CHAIN` holds one entry per
 * interpreter->JIT ENTRY and compiled self-calls never re-enter. 64 activations
 * reported as one frame to `Throwable.getStackTrace()` and `StackWalker` alike.
 *
 * Early rounds run interpreted and report the true depth; later rounds run
 * compiled. Both must agree, and both must agree with HotSpot. Tail and
 * non-tail recursion are measured separately because the collapse hit both,
 * and a distinct-method chain is the control that never collapsed.
 */
public final class JitSelfRecursionFramesProbe {
    private static final int DEPTH = 64;
    private static final int ROUNDS = 40;

    public static void main(String[] args) {
        int firstTail = -1, firstNonTail = -1, firstChain = -1;
        int lastTail = -1, lastNonTail = -1, lastChain = -1;
        for (int i = 0; i < ROUNDS; i++) {
            int t = tailDepth(DEPTH), n = nonTailDepth(DEPTH), c = chainDepth();
            if (i == 0) { firstTail = t; firstNonTail = n; firstChain = c; }
            lastTail = t; lastNonTail = n; lastChain = c;
        }
        System.out.println("first tail=" + firstTail + " nonTail=" + firstNonTail
                + " chain=" + firstChain);
        System.out.println("last tail=" + lastTail + " nonTail=" + lastNonTail
                + " chain=" + lastChain);
        // The computed answers must be untouched by any of this.
        boolean values = sum(100) == 5050 && fact(12) == 479001600;
        System.out.println("values=" + values);
        boolean ok = firstTail == lastTail
                && firstNonTail == lastNonTail
                && firstChain == lastChain
                && lastTail >= DEPTH
                && lastNonTail >= DEPTH
                && values;
        System.out.println(ok ? "SELFREC_FRAMES_OK" : "SELFREC_FRAMES_COLLAPSED");
    }

    static int tailDepth(int d) {
        try { tail(d); return -1; }
        catch (IllegalStateException e) { return e.getStackTrace().length; }
    }
    static void tail(int d) { if (d == 0) throw new IllegalStateException("t"); tail(d - 1); }

    static int nonTailDepth(int d) {
        try { return nonTail(d); }
        catch (IllegalStateException e) { return e.getStackTrace().length; }
    }
    static int nonTail(int d) {
        if (d == 0) throw new IllegalStateException("n");
        return nonTail(d - 1) + 1;
    }

    static int chainDepth() {
        try { c0(); return -1; }
        catch (IllegalStateException e) { return e.getStackTrace().length; }
    }
    static void c0() { c1(); }
    static void c1() { c2(); }
    static void c2() { c3(); }
    static void c3() { throw new IllegalStateException("c"); }

    static int sum(int n) { return n == 0 ? 0 : n + sum(n - 1); }
    static int fact(int n) { return n <= 1 ? 1 : n * fact(n - 1); }
}
