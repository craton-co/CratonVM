// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L1 --
// docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md
// ("Wave 100" section).
//
// GC-soundness check of the quiet blocking-region leave
// (CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT, default on) through the doors
// L1W99BlockingLeaveDepth does not reach: a contended `synchronized` block,
// a contended `static synchronized` method, `Thread.join`, and the wait for a
// class another thread is initializing (wired in wave 100). The main thread
// blocks in each door at the bottom of a shallow (4) and a deep (120)
// recursion. Every frame holds objects allocated before the block: a cell
// the helper mutates exactly once per round while main is blocked, and a
// two-node chain reachable ONLY from that frame. The helper forces
// System.gc() inside every other window, and a hammer thread collects
// throughout, so some leaves fold a collection (full rebuild) and most do not
// (kept snapshot). After the door every frame checks its cell, its chain
// (contents and identity hash) and allocates; a root lost or left stale by a
// kept snapshot reads as bad>0 (or crashes).
//
// No arguments, no setup, no timing in the output:
//   javac -d out L1W100BlockingDoorsLeave.java
//   cratonvm -cp out L1W100BlockingDoorsLeave
//   cratonvm --compatible -cp out L1W100BlockingDoorsLeave
//   (also with --nojit, and with CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT=0)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the probe: checked = depth * rounds)
// monitor depth=4 rounds=6 checked=24 bad=0
// monitor depth=120 rounds=6 checked=720 bad=0
// syncmethod depth=4 rounds=6 checked=24 bad=0
// syncmethod depth=120 rounds=6 checked=720 bad=0
// join depth=4 rounds=6 checked=24 bad=0
// join depth=120 rounds=6 checked=720 bad=0
// clinit depth=4 rounds=6 checked=24 bad=0
// clinit depth=120 rounds=6 checked=720 bad=0
// done
// HOTSPOT_EXPECTED_END

public class L1W100BlockingDoorsLeave {
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

    static final class SyncHolder {
        static synchronized void enter() {
            entered++;
        }
    }

    static final int ROUNDS = 6;
    static final Object LOCK = new Object();
    static volatile long[][] cells;
    static volatile boolean held;
    static volatile boolean clinitStarted;
    static volatile int clinitRound;
    static volatile boolean stop;
    static volatile int entered;
    static Thread waiter;
    static String kind;
    static int checked;
    static int bad;
    static int seq;
    static int initIndex;

    public static void main(String[] args) throws Exception {
        waiter = Thread.currentThread();
        Thread hammer = new Thread(L1W100BlockingDoorsLeave::hammer, "w100-hammer");
        hammer.setDaemon(true);
        hammer.start();
        for (String k : new String[] {"monitor", "syncmethod", "join", "clinit"}) {
            for (int depth : new int[] {4, 120}) {
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
        hammer.join();
        System.out.println("done");
    }

    static void descend(int depth, int max, int round, long[][] row) throws Exception {
        long[] cell = new long[] {depth * 1000L + round};
        Node local = new Node(depth, round);
        local.next = new Node(depth + 1, round);
        int identity = System.identityHashCode(local);
        row[depth] = cell;
        if (depth + 1 < max) {
            descend(depth + 1, max, round, row);
        } else {
            blockInDoor(round, row);
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

    static void blockInDoor(int round, long[][] row) throws Exception {
        cells = row;
        Thread h;
        switch (kind) {
            case "monitor":
                held = false;
                h = new Thread(() -> {
                    synchronized (LOCK) {
                        held = true;
                        awaitState(Thread.State.BLOCKED);
                        mutate(round);
                    }
                }, "w100-holder");
                h.start();
                while (!held) {
                    Thread.onSpinWait();
                }
                synchronized (LOCK) {
                    entered++;
                }
                h.join();
                break;
            case "syncmethod":
                held = false;
                h = new Thread(() -> {
                    synchronized (SyncHolder.class) {
                        held = true;
                        awaitState(Thread.State.BLOCKED);
                        mutate(round);
                    }
                }, "w100-class-holder");
                h.start();
                while (!held) {
                    Thread.onSpinWait();
                }
                SyncHolder.enter();
                h.join();
                break;
            case "join":
                h = new Thread(() -> {
                    awaitState(Thread.State.WAITING);
                    mutate(round);
                }, "w100-joined");
                h.start();
                h.join();
                break;
            default: {
                int index = initIndex++;
                clinitStarted = false;
                clinitRound = round;
                h = new Thread(() -> touchInit(index), "w100-initializer");
                h.start();
                while (!clinitStarted) {
                    Thread.onSpinWait();
                }
                // Blocks until the initializer's <clinit> (which mutates the
                // cells while this thread waits) has finished.
                touchInit(index);
                h.join();
                break;
            }
        }
    }

    // The thread whose <clinit> runs: publish, give the waiter time to block
    // on the initialization, then mutate.
    static void clinitBody() {
        clinitStarted = true;
        try {
            Thread.sleep(30);
        } catch (InterruptedException e) {
            throw new AssertionError(e);
        }
        mutate(clinitRound);
    }

    static void awaitState(Thread.State state) {
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (waiter.getState() != state && System.nanoTime() < deadline) {
            Thread.onSpinWait();
        }
    }

    // Every published cell exactly once per round, while the waiter is
    // blocked; a collection inside every other window.
    static void mutate(int round) {
        for (long[] c : cells) {
            c[0]++;
        }
        churn();
        if ((round & 1) == 0) {
            System.gc();
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

    static void touchInit(int index) {
        switch (index) {
            case 0: Init0.touch(); break;
            case 1: Init1.touch(); break;
            case 2: Init2.touch(); break;
            case 3: Init3.touch(); break;
            case 4: Init4.touch(); break;
            case 5: Init5.touch(); break;
            case 6: Init6.touch(); break;
            case 7: Init7.touch(); break;
            case 8: Init8.touch(); break;
            case 9: Init9.touch(); break;
            case 10: Init10.touch(); break;
            default: Init11.touch(); break;
        }
    }

    static final class Init0 { static { clinitBody(); } static void touch() { } }
    static final class Init1 { static { clinitBody(); } static void touch() { } }
    static final class Init2 { static { clinitBody(); } static void touch() { } }
    static final class Init3 { static { clinitBody(); } static void touch() { } }
    static final class Init4 { static { clinitBody(); } static void touch() { } }
    static final class Init5 { static { clinitBody(); } static void touch() { } }
    static final class Init6 { static { clinitBody(); } static void touch() { } }
    static final class Init7 { static { clinitBody(); } static void touch() { } }
    static final class Init8 { static { clinitBody(); } static void touch() { } }
    static final class Init9 { static { clinitBody(); } static void touch() { } }
    static final class Init10 { static { clinitBody(); } static void touch() { } }
    static final class Init11 { static { clinitBody(); } static void touch() { } }
}
