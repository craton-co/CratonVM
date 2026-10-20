// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 99, lane L1 --
// docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md
//
// GC-soundness check of the quiet blocking-region leave
// (CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT, default on): the leave of a
// sleep / Object.wait region keeps the root snapshot its entry deposited
// when nothing changed in the window. The main thread waits (Thread.sleep(1)
// loops, Object.wait(5) loops, and LockSupport.parkNanos loops for contrast)
// at the bottom of a shallow (4) and a deep (160) recursion. Every frame holds
// objects allocated before the wait: a cell the helper thread mutates exactly
// once per round while main waits, and a two-node chain reachable ONLY from
// that frame. The helper forces System.gc() on every other round inside the
// window, and a hammer thread forces collections throughout, so some leaves
// fold a collection (full rebuild) and most do not (kept snapshot), and
// collections also land right after kept leaves. After the wait every frame
// checks its cell (mutated once), its chain (contents and identity hash) and
// allocates; a frame root lost or left stale by a kept snapshot reads as
// bad>0 (or crashes).
//
// No arguments, no setup, no timing in the output:
//   javac -d out L1W99BlockingLeaveDepth.java
//   cratonvm -cp out L1W99BlockingLeaveDepth
//   cratonvm --compatible -cp out L1W99BlockingLeaveDepth
//   (also with --nojit, and with CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT=0)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the probe: checked = depth * rounds)
// sleep depth=4 rounds=10 checked=40 bad=0
// sleep depth=160 rounds=10 checked=1600 bad=0
// wait depth=4 rounds=10 checked=40 bad=0
// wait depth=160 rounds=10 checked=1600 bad=0
// park depth=4 rounds=10 checked=40 bad=0
// park depth=160 rounds=10 checked=1600 bad=0
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.locks.LockSupport;

public class L1W99BlockingLeaveDepth {
    static final class Node {
        final int depth;
        final int round;
        final long payload;
        Node next;

        Node(int depth, int round) {
            this.depth = depth;
            this.round = round;
            this.payload = depth * 1_000_003L + round;
        }
    }

    static final int ROUNDS = 10;
    static final Object LOCK = new Object();
    static volatile long[][] cells;
    static volatile int requestRound = -1;
    static volatile int doneRound = -1;
    static volatile boolean stop;
    static Thread waiter;
    static String kind;
    static int checked;
    static int bad;
    static int seq;

    public static void main(String[] args) throws Exception {
        waiter = Thread.currentThread();
        Thread helper = new Thread(L1W99BlockingLeaveDepth::helper, "w99-helper");
        Thread hammer = new Thread(L1W99BlockingLeaveDepth::hammer, "w99-hammer");
        helper.setDaemon(true);
        hammer.setDaemon(true);
        helper.start();
        hammer.start();
        for (String k : new String[] {"sleep", "wait", "park"}) {
            for (int depth : new int[] {4, 160}) {
                kind = k;
                checked = 0;
                bad = 0;
                for (int r = 0; r < ROUNDS; r++) {
                    int round = seq++;
                    long[][] row = new long[depth][];
                    descend(0, depth, round, row);
                }
                System.out.println(k + " depth=" + depth + " rounds=" + ROUNDS
                        + " checked=" + checked + " bad=" + bad);
            }
        }
        stop = true;
        LockSupport.unpark(helper);
        helper.join();
        hammer.join();
        System.out.println("done");
    }

    static void descend(int depth, int max, int round, long[][] row) {
        long[] cell = new long[] {depth * 1000L + round};
        Node local = new Node(depth, round);
        local.next = new Node(depth + 1, round);
        int identity = System.identityHashCode(local);
        row[depth] = cell;
        if (depth + 1 < max) {
            descend(depth + 1, max, round, row);
        } else {
            waitForHelper(round, row);
        }
        checked++;
        if (cell[0] != depth * 1000L + round + 1) {
            bad++;
        }
        if (local.payload != depth * 1_000_003L + round
                || local.depth != depth
                || local.next.depth != depth + 1
                || local.next.round != round
                || local.next.payload != (depth + 1) * 1_000_003L + round
                || System.identityHashCode(local) != identity) {
            bad++;
        }
        Object[] junk = new Object[8];
        for (int i = 0; i < junk.length; i++) {
            junk[i] = new byte[128];
        }
        if (((byte[]) junk[7]).length != 128) {
            bad++;
        }
    }

    static void waitForHelper(int round, long[][] row) {
        cells = row;
        requestRound = round;
        switch (kind) {
            case "sleep":
                while (doneRound != round) {
                    try {
                        Thread.sleep(1);
                    } catch (InterruptedException e) {
                        throw new AssertionError(e);
                    }
                }
                break;
            case "wait":
                synchronized (LOCK) {
                    while (doneRound != round) {
                        try {
                            LOCK.wait(5);
                        } catch (InterruptedException e) {
                            throw new AssertionError(e);
                        }
                    }
                }
                break;
            default:
                while (doneRound != round) {
                    LockSupport.parkNanos(2_000_000L);
                }
                break;
        }
    }

    // Mutates every published cell exactly once per round, while the waiter
    // waits for `doneRound`; forces a collection inside every other window.
    static void helper() {
        int seen = -1;
        while (!stop) {
            int r = requestRound;
            if (r == seen) {
                LockSupport.parkNanos(200_000L);
                continue;
            }
            seen = r;
            long[][] row = cells;
            for (long[] c : row) {
                c[0]++;
            }
            churn();
            if ((r & 1) == 0) {
                System.gc();
            }
            doneRound = r;
            LockSupport.unpark(waiter);
            synchronized (LOCK) {
                LOCK.notifyAll();
            }
        }
    }

    static void hammer() {
        while (!stop) {
            churn();
            System.gc();
            try {
                Thread.sleep(15);
            } catch (InterruptedException e) {
                return;
            }
        }
    }

    static Object sink;

    static void churn() {
        Object[] keep = new Object[64];
        for (int i = 0; i < 4096; i++) {
            keep[i & 63] = new int[16];
        }
        sink = keep;
    }
}
