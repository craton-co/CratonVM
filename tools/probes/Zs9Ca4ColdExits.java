// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s9/ca4 (2026-10-16): the exits of a compiled allocation site whose slow
// half lives out of line (`CRATONVM_JIT_ZGC_INLINE_ARM_FALL_THROUGH=1`): the
// stub's exception exit and its stack-trace attribution, from a hot loop.
//
//   nase  a hot `new long[n]` loop, then n = -1: the bump's length screen
//         sends it to the stub, which raises NegativeArraySizeException; the
//         exception leaves through the cold half's exit row. Prints the
//         exception, its message and the top frame's method and line.
//   oome  a hot loop that retains `new long[64]` and `new Object()` until the
//         heap is full (run with -Xmx64m); prints the OOME and the top frame
//         that belongs to this class.
//   mix   both allocation arms in one loop, checked: every array has its
//         length and zeroed body, every object its class.
//
// stdout is deterministic and must equal HotSpot's (`java -XX:+UseZGC`).
//
// Usage: Zs9Ca4ColdExits nase|oome|mix
import java.util.ArrayList;
import java.util.List;

public final class Zs9Ca4ColdExits {
    static long[] mk(int n) {
        long[] last = null;
        for (int i = 0; i < 64; i++) {
            last = new long[n];
        }
        return last;
    }

    static int fill(List<Object[]> chunks) {
        int made = 0;
        while (true) {
            Object[] ring = new Object[4096];
            chunks.add(ring);
            for (int i = 0; i < ring.length; i++) {
                ring[i] = (i & 1) == 0 ? new long[64] : new Object();
                made++;
            }
        }
    }

    static long mix(int rounds) {
        long bad = 0;
        Object[] ring = new Object[1024];
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < 64; k++) {
                int len = (r + k) & 15;
                ring[k] = (k & 1) == 0 ? new long[len] : new Object();
            }
            for (int k = 0; k < 64; k++) {
                Object o = ring[k];
                if ((k & 1) == 0) {
                    long[] a = (long[]) o;
                    if (a.length != ((r + k) & 15)) {
                        bad++;
                    }
                    for (long v : a) {
                        if (v != 0) {
                            bad++;
                        }
                    }
                } else if (o.getClass() != Object.class) {
                    bad++;
                }
            }
        }
        return bad;
    }

    static StackTraceElement ownTop(Throwable t) {
        for (StackTraceElement e : t.getStackTrace()) {
            if (e.getClassName().equals("Zs9Ca4ColdExits")) {
                return e;
            }
        }
        return null;
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "nase";
        switch (mode) {
            case "nase" -> {
                long sum = 0;
                for (int i = 0; i < 200_000; i++) {
                    sum += mk(3).length;
                }
                for (int round = 0; round < 3; round++) {
                    try {
                        mk(-1);
                        System.out.println("nase round=" + round + " no exception");
                    } catch (NegativeArraySizeException e) {
                        StackTraceElement top = ownTop(e);
                        System.out.println("nase round=" + round + " " + e.getClass().getName()
                                + " msg=" + e.getMessage() + " at="
                                + (top == null ? "none" : top.getMethodName() + ":" + top.getLineNumber()));
                    }
                }
                System.out.println("nase warm_sum=" + sum);
            }
            case "oome" -> {
                List<Object[]> chunks = new ArrayList<>();
                try {
                    fill(chunks);
                    System.out.println("oome none");
                } catch (OutOfMemoryError e) {
                    int n = chunks.size();
                    chunks.clear();
                    StackTraceElement top = ownTop(e);
                    System.out.println("oome " + e.getClass().getName() + " at="
                            + (top == null ? "none" : top.getMethodName())
                            + " chunks_positive=" + (n > 0));
                }
            }
            default -> {
                long bad = mix(200_000);
                System.out.println("mix bad=" + bad + " ok=" + (bad == 0));
            }
        }
    }
}
