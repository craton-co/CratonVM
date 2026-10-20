// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicInteger;

/**
 * Netty's {@code new FastThreadLocal<Boolean>()} in miniature: a constructor
 * whose only work is a static wrapper around {@code AtomicInteger.getAndIncrement()}
 * with a cold {@code athrow} arm. The wrapper must stay its own compile so the
 * atomic ladder turns the call into one locked instruction; spliced into the
 * loop it became the JDK's {@code Unsafe.getAndAddInt} retry loop, 15x slower.
 */
public final class AtomicWrapperSpliceProbe {
    static final AtomicInteger NEXT = new AtomicInteger();
    static int sink;

    static int nextIndex() {
        int index = NEXT.getAndIncrement();
        if (index >= Integer.MAX_VALUE - 8 || index < 0) {
            NEXT.set(Integer.MAX_VALUE - 8);
            throw new IllegalStateException("too many indexed variables");
        }
        return index;
    }

    static final class Box {
        final int index;

        Box() {
            index = nextIndex();
        }
    }

    static void loop(int n) {
        for (int j = 0; j < n; j++) {
            new Box();
        }
    }

    public static void main(String[] args) {
        int n = 2_000_000;
        for (int rep = 0; rep < 3; rep++) {
            NEXT.set(0);
            loop(n);
            sink += NEXT.get();
        }
        // An escaping instance, so the wrapper is exercised from the interpreter,
        // the single-pass tier and the optimizing tier alike.
        NEXT.set(5);
        Box b = new Box();
        System.out.println("index=" + b.index + " next=" + NEXT.get());
        System.out.println("sum=" + sink);
        NEXT.set(Integer.MAX_VALUE - 8);
        try {
            nextIndex();
            System.out.println("no throw");
        } catch (IllegalStateException e) {
            System.out.println("caught " + e.getMessage() + " next=" + NEXT.get());
        }
        System.out.println("ATOMIC-WRAPPER-OK");
    }
}
