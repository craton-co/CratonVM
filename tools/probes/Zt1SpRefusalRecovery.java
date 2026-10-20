// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t1/sp (2026-10-19): a program that catches its OutOfMemoryError and goes
// on allocating, under the opt-in final-refusal rule
// (CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS). The rule stops small free-list holes
// from serving after the refusal an OutOfMemoryError is built for, until a
// collection changes the picture; this probe checks that a program which DOES
// change the picture (drops what it filled) recovers exactly as on HotSpot,
// and reports on stderr what one that does not change it can still allocate.
//
// Each mode fills the heap down to object granularity (a chain of Object[2]
// nodes each holding a long[size], size halved on every error, then plain
// Objects), as the L4 launcher probe Zs1NdLauncherOome does:
//   drop  -- then drops the whole chain and allocates 20000 Object[2] plus a
//            1 MiB byte[] (all held): must succeed.
//   half  -- then drops every second node of the chain (about half the heap)
//            and allocates 20000 Object[2] plus a 256 KiB byte[] (held): must
//            succeed.
//   keep  -- keeps the chain and makes 1000 attempts at `new Object()`, each
//            in its own try; the count of served attempts goes to stderr only
//            (it depends on the collector's hole policy: the rule's subject).
//
// stdout (compare byte for byte with java -XX:+UseZGC on the same -Xmx):
//   zt1sp <mode> start
//   zt1sp <mode> filled=true
//   zt1sp <mode> recovered=true            (drop, half)
//   zt1sp <mode> done                      (keep)
// stderr: [probe] lines with counts (not compared).
//
// Usage: Zt1SpRefusalRecovery drop|half|keep      (run at -Xmx32m)
public class Zt1SpRefusalRecovery {
    static Object[] chain;
    static Object[] held;
    static volatile Object sink;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "drop";
        if (!mode.equals("drop") && !mode.equals("half") && !mode.equals("keep")) {
            throw new IllegalArgumentException(mode);
        }
        // Every line is printed while the heap has room: before the fill, or
        // after the mode has dropped (part of) it. A print on the full heap
        // could itself fail, on either VM.
        System.out.println("zt1sp " + mode + " start");
        int nodes = fill();
        boolean filled = chain != null;
        int dropped = 0;
        int served = 0;
        boolean recovered = false;
        if (mode.equals("drop")) {
            chain = null;
            recovered = recover(20000, 1 << 20);
        } else if (mode.equals("half")) {
            dropped = dropEverySecond();
            recovered = recover(20000, 256 << 10);
        } else {
            served = attemptSmall(1000);
        }
        chain = null;
        held = null;
        System.out.println("zt1sp " + mode + " filled=" + filled);
        if (mode.equals("keep")) {
            System.out.println("zt1sp " + mode + " done");
        } else {
            System.out.println("zt1sp " + mode + " recovered=" + recovered);
        }
        System.err.println("[probe] zt1sp " + mode + " nodes=" + nodes + " dropped=" + dropped
                + " served=" + served);
    }

    /** The launcher probe's fill, into the static {@link #chain}. */
    static int fill() {
        int size = 1 << 20;
        int count = 0;
        Object[] head = null;
        try {
            while (true) {
                try {
                    Object[] node = new Object[2];
                    node[0] = size > 0 ? new long[size] : new Object();
                    node[1] = head;
                    head = node;
                    count++;
                } catch (OutOfMemoryError e) {
                    if (size == 0) {
                        break;
                    }
                    size >>= 1;
                }
            }
        } finally {
            chain = head;
        }
        return count;
    }

    /** Unlinks every second node; returns how many were unlinked. */
    static int dropEverySecond() {
        int dropped = 0;
        Object[] node = chain;
        while (node != null) {
            Object[] next = (Object[]) node[1];
            if (next == null) {
                break;
            }
            node[1] = next[1];
            dropped++;
            node = (Object[]) node[1];
        }
        return dropped;
    }

    static boolean recover(int small, int bigBytes) {
        try {
            Object[] keep = new Object[small + 1];
            for (int i = 0; i < small; i++) {
                keep[i] = new Object[2];
            }
            keep[small] = new byte[bigBytes];
            held = keep;
            return held[small] != null && held[0] != null;
        } catch (OutOfMemoryError e) {
            return false;
        }
    }

    static int attemptSmall(int attempts) {
        int served = 0;
        for (int i = 0; i < attempts; i++) {
            try {
                sink = new Object();
                served++;
            } catch (OutOfMemoryError e) {
                // counted as not served
            }
        }
        sink = null;
        return served;
    }
}
