// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * gen r4w6/oomjit6 (2026-09-24): one minimal, deterministic case per SUSPECTED
 * ROOT KIND of the JIT OOME retention
 * ({@code docs/internal/gaps/gengc-r4w4-final-oome-from-compiled-code-leaves-dropped-data-reachable-20260924.md}):
 * after the program drops a data structure, nothing the VM keeps on its
 * behalf may still reach it.
 *
 * <p>Each case builds a structure, keeps only a {@link WeakReference} to it,
 * drops it through one shape, runs {@code System.gc()} twice and prints
 * {@code PASS <name>} when the reference was cleared, {@code FAIL <name>}
 * when something still reaches the structure. The OOME cases also check the
 * heap is usable again (24 MB of fresh allocation). The data is only ever
 * reached through static fields and callee frames that have returned, never
 * through a local of a frame that is still live, so the expectation does not
 * depend on how the reference implementation computes frame liveness.
 *
 * <ul>
 *   <li>{@code catch-staged-arg}: the dropped structure was the ARGUMENT of
 *       the call that threw, and the catch runs in the (compiled) caller's
 *       frame, which stays live across the collection. Suspected root: the
 *       caller's dead staging words (outgoing reserve, argument buffer,
 *       direct-call service copy).</li>
 *   <li>{@code catch-inline-receiver}: the structure was the receiver of a
 *       small, inlinable getter called just before the throw. Suspected root:
 *       an inlined callee's {@code this} slot.</li>
 *   <li>{@code orphan-exceptional-frame}: the callee that throws has its OWN
 *       non-matching {@code catch} around the throw, so compiled code publishes
 *       a precise exceptional frame holding its locals, and the caller catches
 *       locally. Suspected root: the orphaned frame in the exceptional stash.</li>
 *   <li>{@code oome-throwable-kept}: the program keeps the caught
 *       {@code OutOfMemoryError} and drops the data. Suspected root: the
 *       throwable or its backtrace capturing the program's objects.</li>
 *   <li>{@code oome-compiled-callee}: a chain built by a compiled callee until
 *       the heap is full, the OOME caught by its compiled caller.</li>
 *   <li>{@code oome-list-growth}: the list shape ({@code list.add(new long[128])}
 *       until the heap is full; the OOME may come from the list's own growth).</li>
 *   <li>{@code oome-osr-loop}: the list shape with the loop and the catch in a
 *       method entered ONCE, so only OSR compiles it.</li>
 *   <li>{@code oome-thread-exit}: two threads link a shared chain through a
 *       {@code static synchronized} method until the heap is full, clear it
 *       and exit; the main thread checks.</li>
 * </ul>
 *
 * <p>HotSpot ({@code java -Xmx64m -cp tools/bench GenR4W6JitOomRootProbe})
 * prints, in this order:
 * <pre>
 *   PASS catch-staged-arg
 *   PASS catch-inline-receiver
 *   PASS orphan-exceptional-frame
 *   PASS oome-throwable-kept
 *   PASS oome-compiled-callee
 *   PASS oome-list-growth
 *   PASS oome-osr-loop
 *   PASS oome-thread-exit
 *   PASS all 8
 * </pre>
 * and exits 0. Any {@code FAIL} line, a missing line, a timeout or an abort is
 * a failure; the summary is then {@code FAIL <n> of 8} and the exit code 1.
 *
 * <p>Commands (a 300 s timeout is generous):
 * <pre>
 *   javac -d tools/bench tools/bench/GenR4W6JitOomRootProbe.java
 *   java -Xmx64m -cp tools/bench GenR4W6JitOomRootProbe
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W6JitOomRootProbe
 *   CRATONVM_JIT_THRESHOLD=1 timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W6JitOomRootProbe
 *   # the control:
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m --nojit -cp tools/bench GenR4W6JitOomRootProbe
 *   # the two opt-in fixes of this wave, one at a time:
 *   CRATONVM_JIT_LOCAL_HANDLER_CLEAR_DEAD=1 timeout 300 cratonvm ... GenR4W6JitOomRootProbe
 *   CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS=1 timeout 300 cratonvm ... GenR4W6JitOomRootProbe
 *   # the diagnosis of a FAIL: which root section and structure holds it
 *   CRATONVM_DBG=oldmark-root-census,root-source,root-remap-audit timeout 600 cratonvm ... GenR4W6JitOomRootProbe 2>census.log
 * </pre>
 */
public final class GenR4W6JitOomRootProbe {
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

    /** A holder with a trivially inlinable getter. */
    static final class Box {
        final List<long[]> items;

        Box(List<long[]> items) {
            this.items = items;
        }

        int count() {
            return items.size();
        }
    }

    static int failures;
    static long sink;

    // One static per case, so a leak in one case cannot pin another's data.
    static List<long[]> stagedData;
    static Box inlineData;
    static List<long[]> orphanData;
    static List<long[]> keptData;
    static OutOfMemoryError keptOome;
    static Node chainHead;
    static List<long[]> listData;
    static List<long[]> osrData;
    static volatile Node sharedHead;

    // ------------------------------------------------------------------
    // Verdicts
    // ------------------------------------------------------------------

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

    static void verdict(String name, boolean ok) {
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "PASS " : "FAIL ") + name);
    }

    /** About {@code mb} megabytes of {@code long[128]} blocks in a fresh list. */
    static List<long[]> blocks(int mb) {
        final List<long[]> l = new ArrayList<>();
        for (int i = 0; i < mb * 1024; i++) {
            l.add(new long[128]);
        }
        return l;
    }

    // ------------------------------------------------------------------
    // catch-staged-arg
    // ------------------------------------------------------------------

    static int touchAndThrow(List<long[]> l, boolean fail) {
        final int n = l.size();
        if (fail) {
            throw new IllegalStateException("staged");
        }
        return n;
    }

    /**
     * The data is only the ARGUMENT of the call that throws, and the check
     * runs in this (catching) frame, which is therefore live across it.
     */
    static boolean stagedArgCatchThenCheck(WeakReference<?> ref, boolean fail) {
        try {
            sink += touchAndThrow(stagedData, fail);
        } catch (IllegalStateException e) {
            stagedData = null;
        }
        return ref == null || cleared(ref);
    }

    static void caseStagedArg() {
        for (int i = 0; i < 30_000; i++) {
            stagedData = new ArrayList<>();
            stagedArgCatchThenCheck(null, i % 2 == 0);
        }
        stagedData = blocks(16);
        final WeakReference<List<long[]>> ref = new WeakReference<>(stagedData);
        verdict("catch-staged-arg", stagedArgCatchThenCheck(ref, true));
        stagedData = null;
    }

    // ------------------------------------------------------------------
    // catch-inline-receiver
    // ------------------------------------------------------------------

    static void maybeThrow(boolean fail) {
        if (fail) {
            throw new IllegalArgumentException("inline");
        }
    }

    static boolean inlineReceiverCatchThenCheck(WeakReference<?> ref, boolean fail) {
        try {
            sink += inlineData.count();
            maybeThrow(fail);
        } catch (IllegalArgumentException e) {
            inlineData = null;
        }
        return ref == null || cleared(ref);
    }

    static void caseInlineReceiver() {
        for (int i = 0; i < 30_000; i++) {
            inlineData = new Box(new ArrayList<>());
            inlineReceiverCatchThenCheck(null, i % 2 == 0);
        }
        inlineData = new Box(blocks(16));
        final WeakReference<Box> ref = new WeakReference<>(inlineData);
        verdict("catch-inline-receiver", inlineReceiverCatchThenCheck(ref, true));
        inlineData = null;
    }

    // ------------------------------------------------------------------
    // orphan-exceptional-frame
    // ------------------------------------------------------------------

    /**
     * Throws from INSIDE a protected range whose handler does not match, so a
     * compiled body publishes its precise exceptional frame (locals included)
     * on the way out.
     */
    static int orphanCallee(List<long[]> big, boolean fail) {
        final List<long[]> mine = big;
        int n = 0;
        try {
            n = mine.size();
            if (fail) {
                throw new IllegalStateException("orphan");
            }
        } catch (UnsupportedOperationException notThisOne) {
            n = -2;
        }
        return n;
    }

    static boolean orphanCatchThenCheck(WeakReference<?> ref, boolean fail) {
        try {
            sink += orphanCallee(orphanData, fail);
        } catch (IllegalStateException e) {
            orphanData = null;
        }
        return ref == null || cleared(ref);
    }

    static void caseOrphanExceptionalFrame() {
        for (int i = 0; i < 30_000; i++) {
            orphanData = new ArrayList<>();
            orphanCatchThenCheck(null, i % 2 == 0);
        }
        orphanData = blocks(16);
        final WeakReference<List<long[]>> ref = new WeakReference<>(orphanData);
        verdict("orphan-exceptional-frame", orphanCatchThenCheck(ref, true));
        orphanData = null;
    }

    // ------------------------------------------------------------------
    // The OOME cases. `cap >= 0` is the warm-up: a synthetic OutOfMemoryError
    // after `cap` blocks, so the catching method is compiled before the real
    // heap-full run takes the same path.
    // ------------------------------------------------------------------

    static void fillList(List<long[]> l, int cap) {
        int n = 0;
        while (true) {
            l.add(new long[128]);
            if (cap >= 0 && ++n >= cap) {
                throw new OutOfMemoryError("warm-up");
            }
        }
    }

    static boolean keptOomeCatch(int cap) {
        try {
            fillList(keptData, cap);
        } catch (OutOfMemoryError e) {
            keptOome = e;
            keptData = null;
        }
        return keptOome != null;
    }

    static void caseOomeThrowableKept() {
        for (int i = 0; i < 20_000; i++) {
            keptData = new ArrayList<>();
            keptOomeCatch(4);
            keptOome = null;
        }
        keptData = new ArrayList<>();
        final WeakReference<List<long[]>> ref = new WeakReference<>(keptData);
        final boolean caught = keptOomeCatch(-1);
        // The throwable is still held while the check runs.
        final boolean ok = caught && keptOome != null && cleared(ref);
        keptOome = null;
        verdict("oome-throwable-kept", ok && usable());
    }

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

    static boolean chainCatch(WeakReference<?> ref, int cap) {
        try {
            long v = 0;
            while (true) {
                chainHead = grow(chainHead, v, 1024, cap);
                v += 1024;
            }
        } catch (OutOfMemoryError e) {
            chainHead = null;
        }
        return ref == null || chainChecked(ref);
    }

    /**
     * gce e2/c: {@code cleared(ref) && usable()}, with which half failed on
     * STDERR (stdout unchanged): {@code cleared=false} is retention;
     * {@code cleared=true usable=false} is a heap left unable to serve 1 MB
     * arrays after the chain was freed (fragmentation, or an allocation-door
     * verdict), a different defect. A separate method so {@code chainCatch}'s
     * compiled frame keeps its shape.
     */
    static boolean chainChecked(WeakReference<?> ref) {
        final boolean c = cleared(ref);
        final boolean u = c && usable();
        if (!(c && u)) {
            try {
                System.err.println("[probe] chainCatch cleared=" + c + " usable=" + u);
            } catch (Throwable ignored) {
                // Diagnostics only.
            }
        }
        return c && u;
    }

    static void caseOomeCompiledCallee() {
        for (int i = 0; i < 20_000; i++) {
            chainHead = new Node(null, i);
            chainCatch(null, 2);
        }
        chainHead = new Node(null, -1);
        final WeakReference<Node> ref = new WeakReference<>(chainHead);
        verdict("oome-compiled-callee", chainCatch(ref, -1));
    }

    static boolean listCatch(WeakReference<?> ref, int cap) {
        try {
            while (true) {
                listData.add(new long[128]);
                if (cap >= 0 && listData.size() >= cap) {
                    throw new OutOfMemoryError("warm-up");
                }
            }
        } catch (OutOfMemoryError e) {
            // Release a sliver, as the wave-4 list shape does, through the
            // list itself.
            for (int i = 0; i < 64 && !listData.isEmpty(); i++) {
                listData.remove(listData.size() - 1);
            }
        }
        listData = null;
        return ref == null || (cleared(ref) && usable());
    }

    static void caseOomeListGrowth() {
        for (int i = 0; i < 20_000; i++) {
            listData = new ArrayList<>();
            listCatch(null, 3);
        }
        listData = new ArrayList<>();
        final WeakReference<List<long[]>> ref = new WeakReference<>(listData);
        verdict("oome-list-growth", listCatch(ref, -1));
    }

    static long[] osrFirst;

    /**
     * The first block the OSR case's list will hold, published through a
     * static and weakly referenced, from a frame that returns at once (so no
     * live local of the caller holds it).
     */
    static WeakReference<long[]> stageOsrFirst() {
        osrFirst = new long[128];
        return new WeakReference<>(osrFirst);
    }

    static void caseOomeOsrLoop() {
        final WeakReference<long[]> ref = stageOsrFirst();
        verdict("oome-osr-loop", osrListOnceWithFirst(ref));
    }

    /** Entered ONCE: only OSR compiles its loop, and the catch is in it. */
    static boolean osrListOnceWithFirst(WeakReference<?> ref) {
        osrData = new ArrayList<>();
        osrData.add(osrFirst);
        osrFirst = null;
        try {
            while (true) {
                osrData.add(new long[128]);
            }
        } catch (OutOfMemoryError e) {
            for (int i = 0; i < 64 && osrData.size() > 1; i++) {
                osrData.remove(osrData.size() - 1);
            }
        }
        osrData = null;
        return cleared(ref) && usable();
    }

    // ------------------------------------------------------------------
    // oome-thread-exit
    // ------------------------------------------------------------------

    static synchronized void link(long v) {
        sharedHead = new Node(sharedHead, v);
    }

    static synchronized void clearShared() {
        sharedHead = null;
    }

    static void caseOomeThreadExit() throws InterruptedException {
        sharedHead = new Node(null, -1);
        final WeakReference<Node> ref = new WeakReference<>(sharedHead);
        final Runnable filler = () -> {
            try {
                long v = 0;
                while (true) {
                    link(v++);
                }
            } catch (OutOfMemoryError e) {
                clearShared();
            }
        };
        final Thread t1 = new Thread(filler, "oome-thread-1");
        final Thread t2 = new Thread(filler, "oome-thread-2");
        t1.start();
        t2.start();
        t1.join();
        t2.join();
        sharedHead = null;
        verdict("oome-thread-exit", cleared(ref) && usable());
    }

    // ------------------------------------------------------------------

    interface Case {
        void run() throws Exception;
    }

    static void guarded(String name, Case c) {
        try {
            c.run();
        } catch (OutOfMemoryError again) {
            // A second OutOfMemoryError escaping a case is the retention this
            // probe exists to find.
            stagedData = null;
            inlineData = null;
            orphanData = null;
            keptData = null;
            keptOome = null;
            chainHead = null;
            listData = null;
            osrData = null;
            osrFirst = null;
            sharedHead = null;
            // gce e2/c: an escaping error is not retention; say so on stderr.
            try {
                System.err.println("[probe] " + name + ": OutOfMemoryError escaped the case: " + again);
                again.printStackTrace();
            } catch (Throwable ignored) {
                // Diagnostics only: never let them change the verdict line.
            }
            verdict(name, false);
        } catch (Exception unexpected) {
            verdict(name, false);
        }
    }

    public static void main(String[] args) {
        guarded("catch-staged-arg", GenR4W6JitOomRootProbe::caseStagedArg);
        guarded("catch-inline-receiver", GenR4W6JitOomRootProbe::caseInlineReceiver);
        guarded("orphan-exceptional-frame", GenR4W6JitOomRootProbe::caseOrphanExceptionalFrame);
        guarded("oome-throwable-kept", GenR4W6JitOomRootProbe::caseOomeThrowableKept);
        guarded("oome-compiled-callee", GenR4W6JitOomRootProbe::caseOomeCompiledCallee);
        guarded("oome-list-growth", GenR4W6JitOomRootProbe::caseOomeListGrowth);
        guarded("oome-osr-loop", GenR4W6JitOomRootProbe::caseOomeOsrLoop);
        guarded("oome-thread-exit", GenR4W6JitOomRootProbe::caseOomeThreadExit);
        if (failures == 0) {
            System.out.println("PASS all 8");
        } else {
            System.out.println("FAIL " + failures + " of 8");
            System.exit(1);
        }
    }
}
