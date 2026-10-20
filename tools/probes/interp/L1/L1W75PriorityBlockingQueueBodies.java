// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L1:
//
// * `CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE` (the `JdkPriorityBlockingQueue`
//   HotSpot-alignment row, bit 63, default OFF): `--compatible` over a real JDK
//   no longer registers the nine `PriorityBlockingQueue` stand-ins
//   (`native-collections` `register_priority_blocking_queue_natives`: `<init>()V`,
//   `offer`, `put`, both `poll`s, `take`, `peek`, `size`, `isEmpty`), so the
//   JDK 25 bodies run, as `--jdk-only` already did.
//
// What the stand-ins change (expected from code reading, row off): they keep a
// SORTED array by natural order in the JDK's own `queue` / `size` fields, and
// the JDK bodies they do not replace (`toString`, `iterator`, `toArray`,
// `remove(Object)`, `drainTo`, the `Collection` constructor) treat those fields
// as a binary heap. So `heap-order` prints sorted, `comparator` polls smallest
// first, `objects` cannot compare its elements, `collection-ctor` and
// `remove-then-poll` poll out of order, and a `take()` / timed `poll` waiter
// waits on the queue's monitor in 50 ms slices instead of parking on the
// queue's `notEmpty` condition (`blocker=` / `lock=` differ).
//
// Run: `cratonvm L1W75PriorityBlockingQueueBodies` (jdk-only) and
// `CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE=1 cratonvm --compatible
// L1W75PriorityBlockingQueueBodies` must both print the block below;
// `cratonvm --compatible L1W75PriorityBlockingQueueBodies` (row off) is
// expected to differ on the rows named above.
//
// The expected block is DERIVED from the JDK 25 sources (PriorityBlockingQueue,
// Helpers.collectionToString, AbstractQueuedSynchronizer$ConditionObject), not
// yet captured from the host oracle: refresh it from the host's HotSpot run
// before reading a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// heap-order: toString=[1, 2, 4, 5, 3] iterator=[1, 2, 4, 5, 3] size=5 peek=1
// poll-order: [1, 2, 3, 4, 5] then poll=null isEmpty=true
// comparator: toString=[3, 1, 2] peek=3 polled=[3, 2, 1] comparator-is-reverse=true
// objects: polled=[2, 4, 7, 9]
// collection-ctor: toString=[1, 2, 4, 5, 3] size=5 polled=[1, 2, 3, 4, 5]
// remove-then-poll: removed=true toString=[1, 4, 3, 7, 5, 6] polled=[1, 3, 4, 5, 6, 7]
// drainTo(2): drained=[1, 2] left=[3]
// timed poll on empty: 10 ms=null -1 s=null
// offer(null): java.lang.NullPointerException message=null
// take() on empty: WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject lock-is-blocker=true took=42
// poll(60 s) on empty: TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject lock-is-blocker=true took=43
// take() interrupted while parked: java.lang.InterruptedException
// pending interrupt, element ready: take=java.lang.InterruptedException interrupted-after=false poll(60 s)=java.lang.InterruptedException interrupted-after=false size=1
// stress: taken=4000 sum=3998000
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.Iterator;
import java.util.List;
import java.util.concurrent.PriorityBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

public class L1W75PriorityBlockingQueueBodies {
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    interface Body {
        String run() throws Exception;
    }

    interface Blocking {
        Object run() throws Exception;
    }

    static final class Box {
        final int v;

        Box(int v) {
            this.v = v;
        }
    }

    /** One row: its label, then the body's answer or what it threw. */
    static void row(String label, Body body) {
        String out;
        try {
            out = body.run();
        } catch (Throwable t) {
            out = "threw " + t.getClass().getName();
        }
        System.out.println(label + ": " + out);
    }

    static <E> List<E> drain(PriorityBlockingQueue<E> q) {
        List<E> out = new ArrayList<>();
        for (int i = 0; i < 64; i++) {
            E e = q.poll();
            if (e == null) {
                break;
            }
            out.add(e);
        }
        return out;
    }

    static <E> List<E> iterated(PriorityBlockingQueue<E> q) {
        List<E> out = new ArrayList<>();
        for (Iterator<E> it = q.iterator(); it.hasNext(); ) {
            out.add(it.next());
        }
        return out;
    }

    static boolean waiting(Thread.State s) {
        return s == Thread.State.WAITING || s == Thread.State.TIMED_WAITING;
    }

    static String className(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static String lockClass(String name) {
        if (name == null) {
            return "null";
        }
        int at = name.indexOf('@');
        return at < 0 ? name : name.substring(0, at);
    }

    static void settle(Thread t) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!waiting(t.getState()) && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(50);
    }

    static ThreadInfo reading(Thread t) throws InterruptedException {
        ThreadInfo info = MX.getThreadInfo(t.threadId());
        for (int i = 0; i < 50; i++) {
            Thread.State s = t.getState();
            if (waiting(s) && info != null && info.getThreadState() == s && info.getLockName() != null) {
                break;
            }
            Thread.sleep(2);
            info = MX.getThreadInfo(t.threadId());
        }
        return info;
    }

    /** A thread blocked in {@code body}: its state, JMX lock and park blocker; then {@code release} and what it took. */
    static void waiterRow(String label, Blocking body, Runnable release) throws Exception {
        Object[] took = {"nothing"};
        Thread t = new Thread(() -> {
            try {
                took[0] = body.run();
            } catch (Exception e) {
                took[0] = e.getClass().getName();
            }
        }, label);
        t.setDaemon(true);
        t.start();
        settle(t);
        ThreadInfo info = reading(t);
        Thread.State s = t.getState();
        Object blocker = LockSupport.getBlocker(t);
        boolean same = info != null && info.getThreadState() == s;
        String lockName = info == null ? null : info.getLockName();
        boolean lockIsBlocker = blocker != null && lockName != null
                && lockName.equals(blocker.getClass().getName() + '@'
                        + Integer.toHexString(System.identityHashCode(blocker)));
        release.run();
        t.join(5000);
        System.out.println(label + ": " + s + " jmx-same=" + same
                + " lock=" + lockClass(lockName)
                + " blocker=" + className(blocker)
                + " lock-is-blocker=" + lockIsBlocker
                + " took=" + (t.isAlive() ? "still waiting" : String.valueOf(took[0])));
    }

    public static void main(String[] args) throws Exception {
        // ---- The heap is the JDK's: its order shows through toString / iterator ----
        row("heap-order", () -> {
            PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
            for (int v : new int[] {5, 1, 4, 2, 3}) {
                q.offer(v);
            }
            return "toString=" + q + " iterator=" + iterated(q) + " size=" + q.size() + " peek=" + q.peek();
        });
        row("poll-order", () -> {
            PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
            for (int v : new int[] {5, 1, 4, 2, 3}) {
                q.put(v);
            }
            List<Integer> out = drain(q);
            return out + " then poll=" + q.poll() + " isEmpty=" + q.isEmpty();
        });
        // ---- The queue's comparator orders it ----
        row("comparator", () -> {
            PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>(11, Comparator.reverseOrder());
            for (int v : new int[] {1, 3, 2}) {
                q.offer(v);
            }
            String head = "toString=" + q + " peek=" + q.peek();
            List<Integer> out = drain(q);
            return head + " polled=" + out + " comparator-is-reverse=" + (q.comparator() == Comparator.reverseOrder());
        });
        row("objects", () -> {
            PriorityBlockingQueue<Box> q = new PriorityBlockingQueue<>(4, Comparator.comparingInt((Box b) -> b.v));
            for (int v : new int[] {7, 2, 9, 4}) {
                q.offer(new Box(v));
            }
            List<Integer> out = new ArrayList<>();
            for (Box b : drain(q)) {
                out.add(b.v);
            }
            return "polled=" + out;
        });
        // ---- Heap-shaped JDK bodies, then the stand-in surface ----
        row("collection-ctor", () -> {
            PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>(List.of(5, 1, 4, 2, 3));
            String head = "toString=" + q + " size=" + q.size();
            return head + " polled=" + drain(q);
        });
        row("remove-then-poll", () -> {
            PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
            for (int v = 1; v <= 7; v++) {
                q.offer(v);
            }
            boolean removed = q.remove(Integer.valueOf(2));
            String head = "removed=" + removed + " toString=" + q;
            return head + " polled=" + drain(q);
        });
        row("drainTo(2)", () -> {
            PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
            for (int v : new int[] {3, 1, 2}) {
                q.offer(v);
            }
            List<Integer> to = new ArrayList<>();
            q.drainTo(to, 2);
            return "drained=" + to + " left=" + q;
        });
        row("timed poll on empty", () -> {
            PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
            return "10 ms=" + q.poll(10, TimeUnit.MILLISECONDS) + " -1 s=" + q.poll(-1, TimeUnit.SECONDS);
        });
        row("offer(null)", () -> {
            PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
            try {
                q.offer(null);
                return "no exception";
            } catch (Exception e) {
                return e.getClass().getName() + " message=" + e.getMessage();
            }
        });

        // ---- Blocked consumers: state, JMX lock, park blocker ----
        PriorityBlockingQueue<Integer> a = new PriorityBlockingQueue<>();
        waiterRow("take() on empty", a::take, () -> a.offer(42));
        PriorityBlockingQueue<Integer> b = new PriorityBlockingQueue<>();
        waiterRow("poll(60 s) on empty", () -> b.poll(60, TimeUnit.SECONDS), () -> b.offer(43));

        PriorityBlockingQueue<Integer> c = new PriorityBlockingQueue<>();
        String[] outcome = {"no outcome"};
        Thread interrupted = new Thread(() -> {
            try {
                c.take();
                outcome[0] = "returned";
            } catch (Exception e) {
                outcome[0] = e.getClass().getName();
            }
        }, "int-take");
        interrupted.setDaemon(true);
        interrupted.start();
        settle(interrupted);
        interrupted.interrupt();
        interrupted.join(5000);
        System.out.println("take() interrupted while parked: " + outcome[0]);

        // Interpreter round i3 wave 79, lane L1: `take()` / `poll(t, u)` open
        // with `lock.lockInterruptibly()`, so a pending interrupt raises even
        // with an element ready (and is consumed); the element stays queued.
        PriorityBlockingQueue<Integer> d = new PriorityBlockingQueue<>();
        d.offer(5);
        String takeRow;
        Thread.currentThread().interrupt();
        try {
            takeRow = "returned " + d.take();
        } catch (InterruptedException e) {
            takeRow = e.getClass().getName();
        }
        takeRow += " interrupted-after=" + Thread.interrupted();
        String pollRow;
        Thread.currentThread().interrupt();
        try {
            pollRow = "returned " + d.poll(60, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            pollRow = e.getClass().getName();
        }
        pollRow += " interrupted-after=" + Thread.interrupted();
        System.out.println("pending interrupt, element ready: take=" + takeRow + " poll(60 s)=" + pollRow
                + " size=" + d.size());

        // ---- Two producers, two consumers ----
        PriorityBlockingQueue<Integer> s = new PriorityBlockingQueue<>();
        int perProducer = 2000;
        AtomicInteger taken = new AtomicInteger();
        AtomicLong sum = new AtomicLong();
        Thread[] threads = new Thread[4];
        for (int p = 0; p < 2; p++) {
            threads[p] = new Thread(() -> {
                for (int i = 0; i < perProducer; i++) {
                    s.put(i);
                }
            }, "producer-" + p);
        }
        for (int k = 2; k < 4; k++) {
            threads[k] = new Thread(() -> {
                for (int i = 0; i < perProducer; i++) {
                    try {
                        sum.addAndGet(s.take());
                        taken.incrementAndGet();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }, "consumer-" + k);
        }
        for (Thread t : threads) {
            t.setDaemon(true);
            t.start();
        }
        for (Thread t : threads) {
            t.join(30_000);
        }
        System.out.println("stress: taken=" + taken.get() + " sum=" + sum.get());
        System.out.println("done");
    }
}
