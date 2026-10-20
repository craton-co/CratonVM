// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 120, lane L6 -- correctness of
// CRATONVM_VTHREAD_SLICE_ON_CARRIER (a JDK continuation's slice runs on its
// carrier's own OS thread; stage 1 of proposal
// i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread).
// Each row exercises one piece of per-OS-thread state the carrier and the
// slice now share an OS thread for. The output must be identical with the
// switch off and on, in both modes, JIT and --nojit, and equal HotSpot 25's:
// * soe: a virtual thread recurses into StackOverflowError, catches it, and
//   keeps running (the ceiling counts on from enterSpecial's depth);
// * identity: Thread.currentThread() in a virtual thread across 200 parks is
//   itself, and a platform thread's afterwards is its own;
// * thread-locals: a ThreadLocal set in each of 64 virtual threads reads back
//   its own value after parks, and the main thread's is untouched;
// * ffm: a confined arena's segment is accessible from its own virtual thread
//   after parks and refused from another one (WrongThreadException);
// * gc: 64 virtual threads allocate and park while the main thread calls
//   System.gc(); every list checks out;
// * pinned: synchronized + sleep inside a virtual thread (a pinned park on
//   the carrier) completes and counts exactly;
// * interrupt: a parked virtual thread is interrupted and sees it.
//
// Run, each mode (and --nojit), switch off and on:
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W120SliceOnCarrier
//   CRATONVM_VTHREAD_SLICE_ON_CARRIER=1 cratonvm -Djdk.virtualThreadScheduler.parallelism=2 L6W120SliceOnCarrier
// Census: add CRATONVM_DBG_VTHREAD_CENSUS=1 -- `slices_on_carrier` must be
// non-zero with the switch on (else the admission refused every slice:
// `slices_on_carrier_refused`).
//
// HOTSPOT_EXPECTED_BEGIN
// soe: caught=true after=42
// identity: mismatches=0 platform=true
// thread-locals: ok=64 main=main-value
// ffm: own=12345 other=WrongThreadException
// gc: ok=64
// pinned: count=6400
// interrupt: seen=true
// HOTSPOT_EXPECTED_END

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L6W120SliceOnCarrier {
    static int depth;

    static void recurse() {
        depth++;
        recurse();
    }

    static final ThreadLocal<String> LOCAL = new ThreadLocal<>();

    static void startAll(Thread[] ts) throws InterruptedException {
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
    }

    public static void main(String[] args) throws Exception {
        // soe
        boolean[] caught = {false};
        int[] after = {0};
        Thread soe = Thread.ofVirtual().unstarted(() -> {
            try {
                recurse();
            } catch (StackOverflowError e) {
                caught[0] = true;
            }
            LockSupport.parkNanos(1_000);
            after[0] = 6 * 7;
        });
        soe.start();
        soe.join();
        System.out.println("soe: caught=" + caught[0] + " after=" + after[0]);

        // identity
        AtomicInteger mismatches = new AtomicInteger();
        Thread[] ids = new Thread[16];
        for (int i = 0; i < ids.length; i++) {
            ids[i] = Thread.ofVirtual().unstarted(() -> {
                Thread me = Thread.currentThread();
                for (int k = 0; k < 200; k++) {
                    LockSupport.parkNanos(1_000);
                    if (Thread.currentThread() != me) {
                        mismatches.incrementAndGet();
                    }
                }
            });
        }
        startAll(ids);
        Thread platform = Thread.currentThread();
        System.out.println("identity: mismatches=" + mismatches.get()
                + " platform=" + (Thread.currentThread() == platform && !platform.isVirtual()));

        // thread-locals
        LOCAL.set("main-value");
        AtomicInteger tlOk = new AtomicInteger();
        Thread[] tls = new Thread[64];
        for (int i = 0; i < tls.length; i++) {
            final String mine = "v" + i;
            tls[i] = Thread.ofVirtual().unstarted(() -> {
                LOCAL.set(mine);
                boolean good = true;
                for (int k = 0; k < 20; k++) {
                    LockSupport.parkNanos(1_000);
                    good &= mine.equals(LOCAL.get());
                }
                if (good) {
                    tlOk.incrementAndGet();
                }
            });
        }
        startAll(tls);
        System.out.println("thread-locals: ok=" + tlOk.get() + " main=" + LOCAL.get());

        // ffm
        MemorySegment[] seg = new MemorySegment[1];
        int[] own = {0};
        CountDownLatch made = new CountDownLatch(1);
        CountDownLatch tried = new CountDownLatch(1);
        String[] other = {"none"};
        Thread owner = Thread.ofVirtual().unstarted(() -> {
            try (Arena arena = Arena.ofConfined()) {
                seg[0] = arena.allocate(8);
                seg[0].set(ValueLayout.JAVA_INT, 0, 12345);
                made.countDown();
                for (int k = 0; k < 20; k++) {
                    LockSupport.parkNanos(1_000);
                }
                tried.await();
                own[0] = seg[0].get(ValueLayout.JAVA_INT, 0);
            } catch (InterruptedException e) {
                own[0] = -1;
            }
        });
        Thread intruder = Thread.ofVirtual().unstarted(() -> {
            try {
                made.await();
                seg[0].get(ValueLayout.JAVA_INT, 0);
                other[0] = "read";
            } catch (WrongThreadException e) {
                other[0] = "WrongThreadException";
            } catch (InterruptedException e) {
                other[0] = "interrupted";
            } finally {
                tried.countDown();
            }
        });
        owner.start();
        intruder.start();
        owner.join();
        intruder.join();
        System.out.println("ffm: own=" + own[0] + " other=" + other[0]);

        // gc
        AtomicInteger gcOk = new AtomicInteger();
        Thread[] gcs = new Thread[64];
        for (int i = 0; i < gcs.length; i++) {
            final int seed = i;
            gcs[i] = Thread.ofVirtual().unstarted(() -> {
                List<int[]> keep = new ArrayList<>();
                for (int k = 0; k < 200; k++) {
                    int[] a = new int[64];
                    a[0] = seed;
                    a[63] = k;
                    keep.add(a);
                    if (k % 20 == 0) {
                        LockSupport.parkNanos(1_000);
                    }
                }
                boolean good = keep.size() == 200;
                for (int k = 0; k < keep.size(); k++) {
                    good &= keep.get(k)[0] == seed && keep.get(k)[63] == k;
                }
                if (good) {
                    gcOk.incrementAndGet();
                }
            });
        }
        for (Thread t : gcs) {
            t.start();
        }
        for (int k = 0; k < 5; k++) {
            System.gc();
            Thread.sleep(2);
        }
        for (Thread t : gcs) {
            t.join();
        }
        System.out.println("gc: ok=" + gcOk.get());

        // pinned
        Object lock = new Object();
        int[] count = {0};
        Thread[] pins = new Thread[32];
        for (int i = 0; i < pins.length; i++) {
            pins[i] = Thread.ofVirtual().unstarted(() -> {
                for (int k = 0; k < 200; k++) {
                    synchronized (lock) {
                        count[0]++;
                        if (k % 50 == 0) {
                            try {
                                Thread.sleep(1);
                            } catch (InterruptedException e) {
                                return;
                            }
                        }
                    }
                }
            });
        }
        startAll(pins);
        System.out.println("pinned: count=" + count[0]);

        // interrupt
        boolean[] seen = {false};
        Thread sleeper = Thread.ofVirtual().unstarted(() -> {
            LockSupport.park();
            seen[0] = Thread.currentThread().isInterrupted();
        });
        sleeper.start();
        Thread.sleep(20);
        sleeper.interrupt();
        sleeper.join();
        System.out.println("interrupt: seen=" + seen[0]);
    }
}
