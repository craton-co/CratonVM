// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

import java.util.function.Function;

/** Ten Java arguments: nine captured references plus the SAM argument. */
public final class LambdaWideCaptures {
    private static int calls;
    // Allocate before warming the lambda so the throw check measures its entry
    // path without cold exception-constructor calls or unrelated lambdas.
    private static final IllegalStateException FAILURE = new IllegalStateException("wide");

    private static final class Box {
        final int value;
        Box(int value) { this.value = value; }
    }

    private static Function<Integer, Integer> make(int offset) {
        Box a = new Box(offset + 1), b = new Box(offset + 2), c = new Box(offset + 3);
        Box d = new Box(offset + 4), e = new Box(offset + 5), f = new Box(offset + 6);
        Box g = new Box(offset + 7), h = new Box(offset + 8), i = new Box(offset + 9);
        return x -> {
            calls++;
            if (x == -1) throw FAILURE;
            return a.value + 2*b.value + 3*c.value + 4*d.value + 5*e.value
                    + 6*f.value + 7*g.value + 8*h.value + 9*i.value + x;
        };
    }

    // Both the warm loop and exception check use this same call site. Calling
    // apply directly in a cold exception-only method would test the one-shot
    // path even when the checksum had entered the compiled direct site.
    private static int applyTo(Function<Integer, Integer> function, int value) {
        return function.apply(value);
    }

    // Repeated short Java warm-ups drive the small caller's own entry, rather
    // than only compiling/inlining it inside checksum's long OSR loop.
    public static int warmCaller(int iterations) {
        Function<Integer, Integer> function = make(0);
        int sum = 0;
        for (int n = 0; n < iterations; n++) sum += applyTo(function, n & 255);
        return sum;
    }

    public static long checksum() {
        Function<Integer, Integer> first = make(0);
        Function<Integer, Integer> second = make(10);
        long sum = 0;
        for (int n = 0; n < 200000; n++) {
            int x = n & 255;
            sum += applyTo(first, x);
            sum += applyTo(second, x);
        }
        return sum;
    }

    public static int exceptionOnce() {
        Function<Integer, Integer> function = make(20);
        int before = calls;
        try {
            applyTo(function, -1);
            return -1;
        } catch (IllegalStateException expected) {
            if (expected != FAILURE) throw new AssertionError("wrong exception");
            return calls - before;
        }
    }

    public static void main(String[] args) {
        long got = checksum();
        long expected = 0;
        for (int n = 0; n < 200000; n++) expected += 1020 + 2*(n & 255);
        if (got != expected) throw new AssertionError(got + " != " + expected);
        int exceptionCalls = exceptionOnce();
        if (exceptionCalls != 1) throw new AssertionError("exception calls=" + exceptionCalls);
        System.out.println("WIDE-CAPTURE-OK checksum=" + got + " exception_calls=" + exceptionCalls);
    }
}
