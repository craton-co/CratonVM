// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L6 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i73-L6-proposal-spin-on-the-atomic-permit-before-the-lazy-wait-DONE-20261111.md:
//
//   CRATONVM_PARK_LAZY_SPIN_NS=<n>  a platform thread's park spins on its
//                                   permit for up to n ns (capped at 50 000;
//                                   never on a 1-CPU host) before the running
//                                   phase (CRATONVM_PARK_LAZY, on by default)
//                                   waits on the condvar.
//
// Works with CRATONVM_PARK_ATOMIC_PERMIT off (the permit is an AtomicBool
// either way; the waker's notify finds no condvar waiter while the parker
// spins, so it pays no syscall); with it on, the waker also skips the mutex.
//
// A/B, one binary, interleaved, medians of the per-run values (5-7 runs):
//   cratonvm L6W79ParkSpinBench
//   CRATONVM_PARK_LAZY_SPIN_NS=2000  cratonvm L6W79ParkSpinBench
//   CRATONVM_PARK_LAZY_SPIN_NS=10000 cratonvm L6W79ParkSpinBench
//   CRATONVM_PARK_LAZY_SPIN_NS=50000 cratonvm L6W79ParkSpinBench
//   CRATONVM_PARK_LAZY_SPIN_NS=10000 CRATONVM_PARK_ATOMIC_PERMIT=1 cratonvm L6W79ParkSpinBench
// and the same with --compatible (and --nojit). Also L6W72ParkHandoffBench,
// L6W73ParkPermitBench, L1W71CountDownLatchBench and L6W74ParkNoPollBench
// (its gc-with-parkers and idle-cpu rows) under the same arms.
//
// Rows (stdout, min of REPS):
//   park-pingpong    two threads, LockSupport.park / unpark, ns per round trip;
//   sq-handoff       SynchronousQueue put / take between two threads, ns per
//                    element;
//   lock-condition   ReentrantLock + two Conditions ping-pong, ns per round
//                    trip;
//   idle-cpu         IDLERS threads parked untimed for IDLE_MS: their CPU time,
//                    ns per thread per idle second (the spin runs once per
//                    park, never while parked: must stay ~0 in every arm).
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum park-pingpong=8000 sq-handoff=2000 lock-condition=4000 idle-parkers=16
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

public class L6W79ParkSpinBench {
    static final int REPS = 5;
    static final int ROUNDS = 4_000;
    static final int ELEMENTS = 2_000;
    static final int IDLERS = 16;
    static final long IDLE_MS = 500;

    static volatile int turn;
    static volatile boolean idleStop;

    static long parkPingPong() throws InterruptedException {
        turn = 0;
        long[] count = {0};
        Thread main = Thread.currentThread();
        Thread peer = new Thread(() -> {
            for (int i = 0; i < ROUNDS; i++) {
                while (turn != 1) {
                    LockSupport.park();
                }
                count[0]++;
                turn = 0;
                LockSupport.unpark(main);
            }
        }, "w79-park-peer");
        peer.start();
        long mine = 0;
        for (int i = 0; i < ROUNDS; i++) {
            turn = 1;
            LockSupport.unpark(peer);
            while (turn != 0) {
                LockSupport.park();
            }
            mine++;
        }
        peer.join();
        return mine + count[0];
    }

    static long sqHandoff() throws InterruptedException {
        SynchronousQueue<Integer> q = new SynchronousQueue<>();
        long[] sum = {0};
        Thread taker = new Thread(() -> {
            try {
                for (int i = 0; i < ELEMENTS; i++) {
                    sum[0] += q.take() == i ? 1 : 0;
                }
            } catch (InterruptedException e) {
                // not expected
            }
        }, "w79-sq-taker");
        taker.start();
        for (int i = 0; i < ELEMENTS; i++) {
            q.put(i);
        }
        taker.join();
        return sum[0];
    }

    static long lockCondition() throws InterruptedException {
        ReentrantLock lock = new ReentrantLock();
        Condition ping = lock.newCondition();
        Condition pong = lock.newCondition();
        int[] state = {0};
        long[] count = {0};
        Thread peer = new Thread(() -> {
            for (int i = 0; i < ELEMENTS; i++) {
                lock.lock();
                try {
                    while (state[0] != 1) {
                        ping.awaitUninterruptibly();
                    }
                    state[0] = 0;
                    count[0]++;
                    pong.signal();
                } finally {
                    lock.unlock();
                }
            }
        }, "w79-cond-peer");
        peer.start();
        long mine = 0;
        for (int i = 0; i < ELEMENTS; i++) {
            lock.lock();
            try {
                state[0] = 1;
                ping.signal();
                while (state[0] != 0) {
                    pong.awaitUninterruptibly();
                }
                mine++;
            } finally {
                lock.unlock();
            }
        }
        peer.join();
        return mine + count[0];
    }

    /** CPU ns per idle thread per idle second; `parked[0]` gets the count. */
    static double idleCpu(int[] parked) throws InterruptedException {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        Thread[] idlers = new Thread[IDLERS];
        idleStop = false;
        for (int i = 0; i < IDLERS; i++) {
            idlers[i] = new Thread(() -> {
                while (!idleStop) {
                    LockSupport.park();
                }
            }, "w79-idler-" + i);
            idlers[i].setDaemon(true);
            idlers[i].start();
        }
        Thread.sleep(50);
        long[] before = new long[IDLERS];
        for (int i = 0; i < IDLERS; i++) {
            before[i] = mx.isThreadCpuTimeSupported() ? mx.getThreadCpuTime(idlers[i].threadId()) : 0;
        }
        Thread.sleep(IDLE_MS);
        long cpu = 0;
        int waiting = 0;
        for (int i = 0; i < IDLERS; i++) {
            long after = mx.isThreadCpuTimeSupported() ? mx.getThreadCpuTime(idlers[i].threadId()) : 0;
            cpu += Math.max(0, after - before[i]);
            if (idlers[i].getState() == Thread.State.WAITING) {
                waiting++;
            }
        }
        idleStop = true;
        for (Thread t : idlers) {
            LockSupport.unpark(t);
            t.join();
        }
        parked[0] = waiting;
        return cpu / (double) IDLERS * (1000.0 / IDLE_MS);
    }

    interface Row {
        long run() throws InterruptedException;
    }

    static long bench(String name, int per, Row row, long[] checksum) throws InterruptedException {
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            checksum[0] = row.run();
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.printf("%-16s %.1f ns/op%n", name, best / (double) per);
        return checksum[0];
    }

    public static void main(String[] args) throws Exception {
        long[] c = {0};
        // Warm-up: one untimed run of each.
        parkPingPong();
        sqHandoff();
        lockCondition();
        long ping = bench("park-pingpong", ROUNDS, L6W79ParkSpinBench::parkPingPong, c);
        long sq = bench("sq-handoff", ELEMENTS, L6W79ParkSpinBench::sqHandoff, c);
        long cond = bench("lock-condition", ELEMENTS, L6W79ParkSpinBench::lockCondition, c);
        int[] parked = {0};
        double idle = idleCpu(parked);
        System.out.printf("%-16s %.1f ns/op%n", "idle-cpu", idle);
        System.out.println("checksum park-pingpong=" + ping + " sq-handoff=" + sq
                + " lock-condition=" + cond + " idle-parkers=" + parked[0]);
        System.out.println("done");
    }
}
