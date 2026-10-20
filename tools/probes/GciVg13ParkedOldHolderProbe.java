// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

/**
 * gci i13/vg: parked virtual threads as OLD-ONLY roots.
 *
 *   holder N R [KEEP]
 *       N virtual threads park (unmounted) holding a Holder in a frame local.
 *       The holders are aged into the old generation by young collections.
 *       Then R rounds: the main thread reaches every holder through a
 *       WeakReference only (so between rounds the parked frame is its one
 *       strong path), stores a fresh YOUNG two-node chain in it, and allocates
 *       (young collections; a rolling ring of KEEP (default 256) 32 KiB arrays
 *       ages into the old generation and dies there, so occupancy majors run;
 *       one System.gc() at R/2). Finally every thread is released and checks
 *       its holder's chain. A young pause that skips the parked frame must
 *       keep the young chain through the holder's card; a major must keep the
 *       holder itself.
 *
 *   executor
 *       Discovery for gci-i11vt item 6: a virtual thread started by an
 *       Executors.newVirtualThreadPerTaskExecutor() that is then abandoned
 *       (never closed, unreachable) parks forever holding an object only its
 *       frame names. The executor, not the root container, tracks the thread
 *       (JDK 25 ThreadPerTaskExecutor), so on HotSpot nothing reaches either
 *       after the executor is dropped. Prints whether each was collected.
 *
 * stdout is a pure function of the arguments; timing and diagnostics go to
 * stderr ("[vg13] ...").
 */
public class GciVg13ParkedOldHolderProbe {
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static final AtomicLong CHECKSUM = new AtomicLong();
    static volatile boolean release;
    static volatile Object sink;
    static WeakReference<?>[] refs;
    static Object[] keep;

    static final class Node {
        final int v;
        final long[] pad;
        final Node next;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
            this.pad = new long[] { v, v * 3L, v * 7L };
        }
    }

    static final class Holder {
        final int id;
        volatile Node child;

        Holder(int id) {
            this.id = id;
        }
    }

    static void garbage(int k) {
        for (int i = 0; i < k; i++) {
            sink = new byte[128 + (i * 37 % 768)];
        }
    }

    static int expectHead(int round, int id) {
        return round * 31 + id;
    }

    static void parked(int id, CountDownLatch ready, int rounds) {
        Holder h = new Holder(id);
        h.child = new Node(-1 - id, null);
        refs[id] = new WeakReference<Holder>(h);
        ready.countDown();
        while (!release) {
            LockSupport.park();
        }
        Node c = h.child;
        int last = rounds - 1;
        if (h.id != id || c == null || c.v != expectHead(last, id) || c.next == null
                || c.next.v != last || c.pad.length != 3 || c.pad[1] != c.v * 3L
                || c.next.pad[2] != last * 7L) {
            MISMATCH.incrementAndGet();
            // gci i17/hw: classify the mismatch (stderr only; stdout is
            // unchanged). weak=same: the frame and the WeakReference name one
            // object, so its chain itself is wrong (a lost or corrupted
            // field); weak=split: the two name different objects (a stale
            // copy on one side); weak=cleared: the referent was cleared.
            String weak;
            String weakChain;
            try {
                Object wo = refs[id].get();
                weak = wo == null ? "cleared" : (wo == h ? "same" : "split");
                weakChain = (wo instanceof Holder) ? describe(((Holder) wo).child) : "-";
            } catch (Throwable t) {
                weak = "threw:" + t.getClass().getName();
                weakChain = "-";
            }
            System.err.println("[vg13] holder mismatch id=" + id + " h.id=" + h.id + " weak=" + weak
                    + " frame_chain=" + describe(c) + " weak_chain=" + weakChain
                    + " expect_v=" + expectHead(last, id) + " expect_round=" + last);
        } else {
            CHECKSUM.addAndGet(c.v * 7L + c.next.v);
        }
    }

    // gci i17/hw: a chain as the mismatch diagnostic prints it. Never throws.
    static String describe(Node c) {
        if (c == null) {
            return "null";
        }
        try {
            String next = c.next == null ? "null" : String.valueOf(c.next.v);
            String pad = c.pad == null ? "null"
                    : (c.pad.length == 3 ? c.pad[0] + "/" + c.pad[1] + "/" + c.pad[2]
                            : "len" + c.pad.length);
            return "v=" + c.v + ",next.v=" + next + ",pad=" + pad;
        } catch (Throwable t) {
            return "threw:" + t.getClass().getName();
        }
    }

    // Its own frame, so the holder `get()` returns is not left in main's
    // frame between rounds. Returns 1 when the holder was already cleared.
    static int feed(int id, int round) {
        Object o = refs[id].get();
        if (o == null) {
            return 1;
        }
        Holder h = (Holder) o;
        h.child = new Node(expectHead(round, id), new Node(round, null));
        return 0;
    }

    static void holder(int n, int rounds, int keepSlots) throws Exception {
        refs = new WeakReference<?>[n];
        keep = new Object[Math.max(1, keepSlots)];
        CountDownLatch ready = new CountDownLatch(n);
        List<Thread> ts = new ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            final int idx = i;
            ts.add(Thread.ofVirtual().name("vg13-" + i).start(() -> parked(idx, ready, rounds)));
        }
        ready.await();
        // Age the holders into the old generation (young collections only).
        for (int k = 0; k < 16; k++) {
            garbage(20000);
        }
        int lost = 0;
        long allocNanos = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < n; i++) {
                lost += feed(i, r);
            }
            long t0 = System.nanoTime();
            for (int k = 0; k < 4; k++) {
                garbage(4000);
                keep[(r * 4 + k) % keep.length] = new long[4096];
            }
            allocNanos += System.nanoTime() - t0;
            if (r == rounds / 2) {
                System.gc();
            }
        }
        release = true;
        for (Thread t : ts) {
            LockSupport.unpark(t);
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("vg13 holder threads=" + n + " rounds=" + rounds + " lost=" + lost
                + " mismatches=" + MISMATCH.get() + " checksum=" + CHECKSUM.get());
        System.err.println("[vg13] alloc_ms=" + (allocNanos / 1_000_000) + " threads=" + n
                + " rounds=" + rounds);
    }

    // ------------------------------------------------------------ executor

    static WeakReference<Thread> parkedThread;
    static WeakReference<Object> heldOnlyByFrame;

    static void parkForever(CountDownLatch started) {
        Object mine = new int[] { 7, 11, 13 };
        heldOnlyByFrame = new WeakReference<>(mine);
        parkedThread = new WeakReference<>(Thread.currentThread());
        started.countDown();
        while (true) {
            LockSupport.park();
            if (mine.hashCode() == 0x7fffffff) {
                sink = mine;
            }
        }
    }

    // Its own frame: the executor and the Future die with it.
    static void startInAbandonedExecutor() throws Exception {
        CountDownLatch started = new CountDownLatch(1);
        ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor();
        ex.submit(() -> parkForever(started));
        started.await();
        // Let the task reach its park.
        Thread.sleep(200);
    }

    static void executor() throws Exception {
        startInAbandonedExecutor();
        for (int g = 0; g < 3; g++) {
            garbage(20000);
            System.gc();
            Thread.sleep(100);
        }
        System.out.println("executor parked thread collected=" + (parkedThread.get() == null)
                + " frame-only object collected=" + (heldOnlyByFrame.get() == null));
        System.exit(0);
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "holder";
        switch (mode) {
            case "holder":
                holder(args.length > 1 ? Integer.parseInt(args[1]) : 1000,
                        args.length > 2 ? Integer.parseInt(args[2]) : 200,
                        args.length > 3 ? Integer.parseInt(args[3]) : 256);
                break;
            case "executor":
                executor();
                break;
            default:
                throw new IllegalArgumentException("mode: holder N R [KEEP] | executor");
        }
    }
}
