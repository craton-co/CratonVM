// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 95, lane L6 -- what a JDK virtual thread answers
// across its registration, mounts and death, with the wave-95 lifecycle
// switches off and on:
//   CRATONVM_VTHREAD_ONE_PUBLISH (default off): one registry publish per
//     registration, mount and death; the terminated tail purged by a kept
//     count outside the GC barrier's lock;
//   CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT (default off): the death walks the
//     inflated monitors only for a thread holding a monitor it entered outside
//     structured locking;
//   CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES (default ON, =0 off): terminated
//     JDK continuations leave the virtual-thread manager past a bounded tail
//     (1024 kept, purged past 2048), so the sequential section starts 3000.
// Sections:
// * sequential: 3000 virtual threads started and joined one at a time (past
//   the registry's 256-entry terminated tail and the manager's tail); inside,
//   the thread is itself, virtual and RUNNABLE; after, TERMINATED and dead;
// * oldest: the FIRST of them, 3000 deaths later -- state, isAlive,
//   isVirtual, join, interrupt, getAllStackTraces, ThreadMXBean;
// * contended: 64 virtual threads at once increment one counter under one
//   monitor (contended, so inflated) and wait(1) on another (inflated); after
//   every death, a platform thread locks both within 5 s;
// * unwound: a virtual thread's uncaught exception thrown inside a
//   synchronized block and a synchronized method; the monitors are free after;
// * parked: virtual threads parked across a yield (LockSupport.park, sleep)
//   and remounted, then joined;
// * batch: 5000 tasks on newVirtualThreadPerTaskExecutor, distinct ids.
// The output must be the same with every switch combination, in both modes:
//   cratonvm L6W95VirtualThreadLifecycle
//   CRATONVM_VTHREAD_ONE_PUBLISH=1 cratonvm L6W95VirtualThreadLifecycle
//   CRATONVM_VTHREAD_ONE_PUBLISH=1 CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT=1 cratonvm L6W95VirtualThreadLifecycle
//   CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES=0 cratonvm L6W95VirtualThreadLifecycle
//   (and each with --compatible; add CRATONVM_DBG_VTHREAD_CENSUS=1 to read
//   death_sweep_skip_misses, which must be 0, and the at-exit entry counts)
//
// HOTSPOT_EXPECTED_BEGIN
// sequential: started=3000 self=3000 virtual=3000 runnable=3000 terminated=3000 dead=3000 distinctIds=3000
// oldest: state=TERMINATED alive=false virtual=true joined=true interrupted=true listed=false info=null
// contended: count=6400 waited=64 locked=true
// unwound: handler=2 messages=in-block,in-method free=true
// parked: unparked=32 slept=32
// batch: tasks=5000 distinct=5000 virtual=5000
// live: info=null listed=false
// HOTSPOT_EXPECTED_END

import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L6W95VirtualThreadLifecycle {
    static final Object COUNTER_LOCK = new Object();
    static final Object WAIT_LOCK = new Object();
    static final Object BLOCK_LOCK = new Object();
    static int counter;

    static synchronized void throwInMethod() {
        throw new IllegalStateException("in-method");
    }

    public static void main(String[] args) throws Exception {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();

        // sequential
        int self = 0;
        int virtual = 0;
        int runnable = 0;
        int terminated = 0;
        int dead = 0;
        Set<Long> ids = new HashSet<>();
        Thread first = null;
        int seq = 3000;
        for (int i = 0; i < seq; i++) {
            boolean[] seen = new boolean[3];
            Thread[] holder = new Thread[1];
            Thread t = Thread.ofVirtual().unstarted(() -> {
                Thread me = Thread.currentThread();
                seen[0] = me == holder[0];
                seen[1] = me.isVirtual();
                seen[2] = me.getState() == Thread.State.RUNNABLE;
            });
            holder[0] = t;
            t.start();
            t.join();
            if (seen[0]) {
                self++;
            }
            if (seen[1]) {
                virtual++;
            }
            if (seen[2]) {
                runnable++;
            }
            if (t.getState() == Thread.State.TERMINATED) {
                terminated++;
            }
            if (!t.isAlive()) {
                dead++;
            }
            ids.add(t.threadId());
            if (first == null) {
                first = t;
            }
        }
        System.out.println("sequential: started=" + seq + " self=" + self + " virtual=" + virtual
                + " runnable=" + runnable + " terminated=" + terminated + " dead=" + dead
                + " distinctIds=" + ids.size());

        // oldest
        boolean joined = first.join(java.time.Duration.ofSeconds(5));
        first.interrupt();
        boolean listed = Thread.getAllStackTraces().containsKey(first);
        Object info = mx.getThreadInfo(first.threadId());
        System.out.println("oldest: state=" + first.getState() + " alive=" + first.isAlive()
                + " virtual=" + first.isVirtual() + " joined=" + joined
                + " interrupted=" + first.isInterrupted() + " listed=" + listed
                + " info=" + info);

        // contended
        int n = 64;
        List<Thread> group = new ArrayList<>();
        AtomicInteger waited = new AtomicInteger();
        CountDownLatch go = new CountDownLatch(1);
        for (int i = 0; i < n; i++) {
            group.add(Thread.ofVirtual().start(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                    return;
                }
                for (int k = 0; k < 100; k++) {
                    synchronized (COUNTER_LOCK) {
                        counter++;
                    }
                }
                synchronized (WAIT_LOCK) {
                    try {
                        WAIT_LOCK.wait(1);
                        waited.incrementAndGet();
                    } catch (InterruptedException e) {
                        // not expected
                    }
                }
            }));
        }
        go.countDown();
        for (Thread t : group) {
            t.join();
        }
        boolean locked = lockedWithin(5000, COUNTER_LOCK, WAIT_LOCK);
        System.out.println("contended: count=" + counter + " waited=" + waited.get()
                + " locked=" + locked);

        // unwound
        List<String> messages = new ArrayList<>();
        AtomicInteger handled = new AtomicInteger();
        Thread.UncaughtExceptionHandler handler = (t, e) -> {
            handled.incrementAndGet();
            synchronized (messages) {
                messages.add(e.getMessage());
            }
        };
        Thread inBlock = Thread.ofVirtual().uncaughtExceptionHandler(handler).start(() -> {
            synchronized (BLOCK_LOCK) {
                throw new IllegalStateException("in-block");
            }
        });
        inBlock.join();
        Thread inMethod = Thread.ofVirtual().uncaughtExceptionHandler(handler)
                .start(L6W95VirtualThreadLifecycle::throwInMethod);
        inMethod.join();
        boolean free = lockedWithin(5000, BLOCK_LOCK, L6W95VirtualThreadLifecycle.class);
        List<String> sorted = new ArrayList<>(messages);
        sorted.sort(null);
        System.out.println("unwound: handler=" + handled.get() + " messages="
                + String.join(",", sorted) + " free=" + free);

        // parked
        int parkers = 32;
        AtomicInteger unparked = new AtomicInteger();
        AtomicInteger slept = new AtomicInteger();
        List<Thread> parked = new ArrayList<>();
        CountDownLatch parking = new CountDownLatch(parkers);
        Set<Thread> released = ConcurrentHashMap.newKeySet();
        for (int i = 0; i < parkers; i++) {
            parked.add(Thread.ofVirtual().start(() -> {
                parking.countDown();
                while (!released.contains(Thread.currentThread())) {
                    LockSupport.park();
                }
                unparked.incrementAndGet();
                try {
                    Thread.sleep(2);
                    slept.incrementAndGet();
                } catch (InterruptedException e) {
                    // not expected
                }
            }));
        }
        parking.await();
        for (Thread t : parked) {
            released.add(t);
            LockSupport.unpark(t);
        }
        for (Thread t : parked) {
            t.join();
        }
        System.out.println("parked: unparked=" + unparked.get() + " slept=" + slept.get());

        // batch
        int tasks = 5000;
        Set<Long> batchIds = ConcurrentHashMap.newKeySet();
        AtomicInteger batchVirtual = new AtomicInteger();
        try (ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor()) {
            List<Future<?>> fs = new ArrayList<>();
            for (int i = 0; i < tasks; i++) {
                fs.add(ex.submit(() -> {
                    Thread me = Thread.currentThread();
                    batchIds.add(me.threadId());
                    if (me.isVirtual()) {
                        batchVirtual.incrementAndGet();
                    }
                    synchronized (COUNTER_LOCK) {
                        counter++;
                    }
                }));
            }
            for (Future<?> f : fs) {
                f.get();
            }
        }
        System.out.println("batch: tasks=" + tasks + " distinct=" + batchIds.size()
                + " virtual=" + batchVirtual.get());

        // live: a parked live virtual thread is not a ThreadMXBean thread either
        CountDownLatch hold = new CountDownLatch(1);
        Thread live = Thread.ofVirtual().start(() -> {
            try {
                hold.await();
            } catch (InterruptedException e) {
                // not expected
            }
        });
        Object liveInfo = mx.getThreadInfo(live.threadId());
        boolean liveListed = Thread.getAllStackTraces().containsKey(live);
        hold.countDown();
        live.join();
        System.out.println("live: info=" + liveInfo + " listed=" + liveListed);
    }

    /** Whether a platform thread acquires every one of `locks` within `ms`. */
    static boolean lockedWithin(long ms, Object... locks) throws InterruptedException {
        boolean[] done = new boolean[1];
        Thread p = Thread.ofPlatform().daemon(true).start(() -> {
            lockAll(locks, 0);
            done[0] = true;
        });
        p.join(ms);
        return done[0] && !p.isAlive();
    }

    static void lockAll(Object[] locks, int i) {
        if (i == locks.length) {
            return;
        }
        synchronized (locks[i]) {
            lockAll(locks, i + 1);
        }
    }
}
