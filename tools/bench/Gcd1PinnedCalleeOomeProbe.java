// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gcd d5/s (2026-09-28): the {@code oome-compiled-callee} shape of
 * {@code GenR4W6JitOomRootProbe}, alone, with its verdict split into its three
 * parts so a failure says WHICH part failed.
 *
 * <p>A compiled callee ({@code grow}) builds a linked chain until the heap is
 * full; its compiled caller ({@code chainCatch}) catches the
 * {@code OutOfMemoryError}, drops the chain and checks that a
 * {@link WeakReference} to the chain's OLDEST node is cleared after up to three
 * {@code System.gc()} calls, then that 24 MB of fresh allocation succeeds.
 * The oldest node is reachable from every other node, so any single retained
 * node keeps it: the case is maximally sensitive to retention.
 *
 * <p>With {@code CRATONVM_GEN_PINNED_YOUNG_COPY=1} (and
 * {@code CRATONVM_GC_FUTILE_YOUNG_BACKOFF=0}) {@code GenR4W6JitOomRootProbe}
 * printed {@code FAIL oome-compiled-callee} 3 of 3 on {@code d916d1c40} where
 * the default passed; see
 * {@code docs/internal/gc/gcd-d5s-pinned-copy-keeps-a-dropped-chain-across-requested-majors-FIXED-20260928.md}.
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench Gcd1PinnedCalleeOomeProbe})
 * prints, in this order:
 * <pre>
 *   callee-oome caught=true
 *   callee-oome cleared=true
 *   callee-oome usable=true
 *   PASS
 * </pre>
 * and exits 0. Otherwise the last line is {@code FAIL} and the exit code 1.
 * The {@code cleared=} line is the retention verdict; {@code usable=false}
 * with {@code cleared=true} would be a heap that cannot serve 1 MB arrays
 * after the chain is gone (fragmentation, or a young/old sizing gap), not
 * retention.
 *
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1PinnedCalleeOomeProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
 *   CRATONVM_GC_FUTILE_YOUNG_BACKOFF=0 timeout 300 cratonvm $P Gcd1PinnedCalleeOomeProbe
 *   CRATONVM_GC_FUTILE_YOUNG_BACKOFF=0 CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DBG=gc-stats \
 *     timeout 300 cratonvm $P Gcd1PinnedCalleeOomeProbe
 * </pre>
 */
public final class Gcd1PinnedCalleeOomeProbe {
    static final class Node {
        final Node next;
        final long a, b, c, d;

        Node(Node next, long v) {
            this.next = next;
            this.a = v;
            this.b = v + 1;
            this.c = v + 2;
            this.d = v + 3;
        }
    }

    static Node chainHead;
    static long sink;

    /** The compiled callee: {@code n} nodes onto {@code from}. */
    static Node grow(Node from, long v, int n, int cap) {
        Node h = from;
        for (int i = 0; i < n; i++) {
            h = new Node(h, v + i);
            if (cap >= 0 && i >= cap) {
                throw new OutOfMemoryError("warm-up");
            }
        }
        return h;
    }

    static boolean caught;
    static boolean isCleared;
    static boolean isUsable;

    /**
     * The compiled caller: grow until the heap is full, drop the chain, and
     * run the checks IN THIS FRAME (live across them), as
     * {@code GenR4W6JitOomRootProbe.chainCatch} does.
     */
    static void chainCatch(WeakReference<?> ref, int cap) {
        try {
            long v = 0;
            while (true) {
                chainHead = grow(chainHead, v, 1024, cap);
                v += 1024;
            }
        } catch (OutOfMemoryError e) {
            chainHead = null;
            caught = true;
        }
        if (ref != null) {
            phase = "cleared";
            isCleared = cleared(ref);
            phase = "usable";
            isUsable = usable();
            phase = "done";
        }
    }

    /**
     * gce e1/o: which check was running when an {@code OutOfMemoryError}
     * escaped {@code chainCatch}. The escape path prints the same
     * {@code cleared=false usable=false} pair as a retention failure; the
     * stderr line below tells them apart (stdout is unchanged).
     */
    static String phase = "grow";

    static boolean cleared(WeakReference<?> ref) {
        for (int i = 0; i < 3 && ref.get() != null; i++) {
            System.gc();
        }
        return ref.get() == null;
    }

    /** 24 MB of fresh, short-lived allocation must succeed. */
    static boolean usable() {
        try {
            long s = 0;
            for (int i = 0; i < 24; i++) {
                final long[] chunk = new long[128 * 1024];
                chunk[i] = i;
                s += chunk[i];
            }
            sink += s;
            return true;
        } catch (OutOfMemoryError again) {
            return false;
        }
    }

    public static void main(String[] args) {
        for (int i = 0; i < 20_000; i++) {
            chainHead = new Node(null, i);
            chainCatch(null, 2);
        }
        caught = false;
        chainHead = new Node(null, -1);
        final WeakReference<Node> ref = new WeakReference<>(chainHead);
        try {
            chainCatch(ref, -1);
        } catch (OutOfMemoryError escaped) {
            // An OutOfMemoryError escaping the checks is the retention too.
            chainHead = null;
            isCleared = false;
            isUsable = false;
            // gce e1/o: diagnostics only, on stderr. `phase=grow` with
            // `caught=false`: the handler never ran; `phase=cleared`: the
            // error came out of `System.gc()` / `ref.get()` (a door, not
            // retention); `phase=usable` cannot escape (it catches its own).
            System.err.println("[probe] OutOfMemoryError escaped chainCatch: phase=" + phase
                    + " caught=" + caught + " message=" + escaped.getMessage());
            escaped.printStackTrace();
        }
        System.out.println("callee-oome caught=" + caught);
        System.out.println("callee-oome cleared=" + isCleared);
        System.out.println("callee-oome usable=" + isUsable);
        final boolean ok = caught && isCleared && isUsable;
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
