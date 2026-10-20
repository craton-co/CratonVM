// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L2 (hunt, SUSPECTED; page
// i101-L2-compiled-frame-without-a-safepoint-id-has-no-line): the stack trace
// of a throwable created or thrown inside COMPILED code must name the same
// frames with the same line numbers as the first, interpreted, run of the
// same code. HotSpot keeps debug info at every call and trap site, so its
// traces never change with the tier. Here a compiled activation's line comes
// from the safepoint id the live frame published
// (stackwalker.rs `compiled_frame_entry`); a frame stopped where no id was
// published reads LINE_NUMBER_UNKNOWN, and an inlined callee's line comes
// from the inline-frame map.
//
// Each case captures a trace on its first call (cold, interpreted) and again
// after 30 000 calls (compiled; OSR for the loop case) and compares the top
// frames below main (class, method, line). Shapes: a Throwable built in a leaf two
// static calls down; an exception thrown by an inlined virtual callee; an
// implicit ArithmeticException in a hot loop body (OSR); a trace taken
// through Thread.getStackTrace() from a compiled caller; an exception that
// crosses a deopt (a new receiver class after warm-up).
//
// Run, each mode, JIT default and --nojit:
//   cratonvm L2W101CompiledTraceLines
// For a `same=false` row, rerun with CRATONVM_DBG_JITC=1: the exit line
// `[cratonvm-jitc] compiled-frame-lines ...` names the refusal population
// (no-sp-id / id-unrecorded / ir-untranslated / out-of-range / switched-off).
//
// HOTSPOT_EXPECTED_BEGIN
// leaf: same=true
// inlined-throw: same=true
// osr-loop: same=true
// getStackTrace: same=true
// deopt-throw: same=true
// HOTSPOT_EXPECTED_END

import java.util.Arrays;

public class L2W101CompiledTraceLines {
    // The top `n` frames: only the probe's own callee frames, never `main`
    // (whose line differs between the cold and the hot call sites).
    static String key(StackTraceElement[] st, int n) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < Math.min(n, st.length); i++) {
            b.append(st[i].getClassName()).append('.').append(st[i].getMethodName())
                    .append(':').append(st[i].getLineNumber()).append(';');
        }
        return b.toString();
    }

    // Case 1: a Throwable built in a leaf two static calls down.
    static Throwable leaf(int i) {
        return new Throwable("t" + (i & 1));
    }

    static Throwable mid(int i) {
        return leaf(i + 1);
    }

    static Throwable top(int i) {
        return mid(i * 2);
    }

    // Case 2: an inlined virtual callee throws.
    static class Box {
        int v;

        int get(int k) {
            if (k < 0) {
                throw new IllegalArgumentException("neg");
            }
            return v + k;
        }
    }

    static int useBox(Box b, int k) {
        return b.get(k) * 2;
    }

    // Case 3: an implicit ArithmeticException in a hot loop (OSR).
    static int divLoop(int n, int zeroAt) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            int d = (i == zeroAt) ? 0 : 1 + (i & 3);
            s += 1000 / d;
        }
        return s;
    }

    // Case 4: Thread.getStackTrace() from a compiled caller.
    static StackTraceElement[] here(int i) {
        return Thread.currentThread().getStackTrace();
    }

    static StackTraceElement[] callsHere(int i) {
        return here(i ^ 1);
    }

    // Case 5: an exception after a deopt.
    static class Base {
        int val(int i) {
            if (i == -5) {
                throw new IllegalStateException("base");
            }
            return i;
        }
    }

    static class Late extends Base {
        @Override
        int val(int i) {
            if (i == -5) {
                throw new IllegalStateException("late");
            }
            return i + 1;
        }
    }

    static int callVal(Base b, int i) {
        return b.val(i) + 1;
    }

    static String trace(Throwable t, int n) {
        return key(t.getStackTrace(), n);
    }

    public static void main(String[] args) {
        // 1
        String cold = trace(top(0), 3);
        String hot = null;
        for (int i = 0; i < 30_000; i++) {
            Throwable t = top(0);
            if (i == 29_999) {
                hot = trace(t, 3);
            }
        }
        System.out.println("leaf: same=" + cold.equals(hot));

        // 2
        Box box = new Box();
        String cold2 = null;
        try {
            useBox(box, -1);
        } catch (IllegalArgumentException e) {
            cold2 = trace(e, 2);
        }
        long sink = 0;
        for (int i = 0; i < 30_000; i++) {
            sink += useBox(box, i & 7);
        }
        String hot2 = null;
        try {
            useBox(box, -1);
        } catch (IllegalArgumentException e) {
            hot2 = trace(e, 2);
        }
        System.out.println("inlined-throw: same=" + (cold2 != null && cold2.equals(hot2)));

        // 3
        String cold3 = null;
        try {
            divLoop(10, 3);
        } catch (ArithmeticException e) {
            cold3 = trace(e, 1);
        }
        String hot3 = null;
        try {
            divLoop(2_000_000, 1_999_000);
        } catch (ArithmeticException e) {
            hot3 = trace(e, 1);
        }
        System.out.println("osr-loop: same=" + (cold3 != null && cold3.equals(hot3)));

        // 4: drop frame 0 (Thread.getStackTrace itself) from both.
        String cold4 = key(Arrays.copyOfRange(callsHere(0), 1, 3), 2);
        String hot4 = null;
        for (int i = 0; i < 30_000; i++) {
            StackTraceElement[] st = callsHere(0);
            if (i == 29_999) {
                hot4 = key(Arrays.copyOfRange(st, 1, 3), 2);
            }
        }
        System.out.println("getStackTrace: same=" + cold4.equals(hot4));

        // 5
        Base b = new Base();
        String cold5 = null;
        try {
            callVal(b, -5);
        } catch (IllegalStateException e) {
            cold5 = trace(e, 2);
        }
        for (int i = 0; i < 30_000; i++) {
            sink += callVal(b, i);
        }
        sink += callVal(new Late(), 3);
        String hot5 = null;
        try {
            callVal(b, -5);
        } catch (IllegalStateException e) {
            hot5 = trace(e, 2);
        }
        System.out.println("deopt-throw: same=" + (cold5 != null && cold5.equals(hot5)));
        if (sink == 42) {
            System.out.println("unlikely");
        }
    }
}
