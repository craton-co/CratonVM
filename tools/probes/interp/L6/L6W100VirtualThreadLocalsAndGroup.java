// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L6 -- ThreadLocal and
// InheritableThreadLocal across virtual threads, and a virtual thread's
// ThreadGroup, against HotSpot 25.
//
//  * A virtual thread has its own ThreadLocal map: it does not see main's
//    value, keeps its own across a park / sleep (a remount, possibly on
//    another carrier), and two virtual threads sharing one carrier do not
//    see each other's.
//  * An InheritableThreadLocal is inherited (through `childValue`) by a
//    virtual thread unless the builder says `inheritInheritableThreadLocals(false)`.
//  * `withInitial` runs once per thread.
//  * `getThreadGroup()` of a live virtual thread is the "VirtualThreads"
//    group (parent "system", max priority 10, daemon-less), asked FIRST from
//    a virtual thread before any platform thread asked (CratonVM's
//    `--compatible` native answers null until a platform thread has
//    initialized `Thread$Constants`); a terminated one answers null; the
//    group's activeCount() and enumerate() list no virtual thread.
//
// The scheduler is pinned to one carrier (parallelism = maxPoolSize = 1).
//
// HOTSPOT_EXPECTED_BEGIN
// first-group-from-virtual: VirtualThreads
// group-parent: system max=10
// main-tl-in-vt: null
// own-tl-after-sleep: vt-a
// own-tl-after-park: vt-a
// neighbour-tl: null
// itl-inherited: main-itl/child
// itl-not-inherited: null
// itl-platform-not-inherited: null
// initial-calls: 3
// group-activeCount: 0
// group-enumerate: 0
// terminated-group: null
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.LockSupport;

public class L6W100VirtualThreadLocalsAndGroup {
    static final ThreadLocal<String> TL = new ThreadLocal<>();
    static final InheritableThreadLocal<String> ITL = new InheritableThreadLocal<>() {
        @Override
        protected String childValue(String parent) {
            return parent == null ? null : parent + "/child";
        }
    };
    static final AtomicInteger INITIALS = new AtomicInteger();
    static final ThreadLocal<Integer> COUNTED = ThreadLocal.withInitial(INITIALS::incrementAndGet);

    static String onVirtual(Thread.Builder builder, java.util.concurrent.Callable<String> c)
            throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        Thread t = builder.start(() -> {
            try {
                out.set(c.call());
            } catch (Exception e) {
                out.set("threw " + e);
            }
        });
        t.join();
        return out.get();
    }

    public static void main(String[] args) throws Exception {
        System.setProperty("jdk.virtualThreadScheduler.parallelism", "1");
        System.setProperty("jdk.virtualThreadScheduler.maxPoolSize", "1");

        // Before anything else asks for a virtual thread's group.
        System.out.println("first-group-from-virtual: " + onVirtual(Thread.ofVirtual(), () -> {
            ThreadGroup g = Thread.currentThread().getThreadGroup();
            return g == null ? "null" : g.getName();
        }));
        System.out.println(onVirtual(Thread.ofVirtual(), () -> {
            ThreadGroup g = Thread.currentThread().getThreadGroup();
            ThreadGroup p = g == null ? null : g.getParent();
            return "group-parent: " + (p == null ? "null" : p.getName())
                + " max=" + (g == null ? -1 : g.getMaxPriority());
        }));

        TL.set("main-tl");
        System.out.println("main-tl-in-vt: " + onVirtual(Thread.ofVirtual(), TL::get));
        System.out.println("own-tl-after-sleep: " + onVirtual(Thread.ofVirtual(), () -> {
            TL.set("vt-a");
            Thread.sleep(2);
            return TL.get();
        }));
        System.out.println("own-tl-after-park: " + onVirtual(Thread.ofVirtual(), () -> {
            TL.set("vt-a");
            LockSupport.parkNanos(1_000_000L);
            Thread.yield();
            return TL.get();
        }));

        // Two virtual threads alternating on the one carrier.
        CountDownLatch aSet = new CountDownLatch(1);
        CountDownLatch bRead = new CountDownLatch(1);
        AtomicReference<String> seen = new AtomicReference<>();
        Thread a = Thread.ofVirtual().start(() -> {
            TL.set("vt-a");
            aSet.countDown();
            try {
                bRead.await();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        Thread b = Thread.ofVirtual().start(() -> {
            try {
                aSet.await();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
            seen.set(TL.get());
            bRead.countDown();
        });
        a.join();
        b.join();
        System.out.println("neighbour-tl: " + seen.get());

        ITL.set("main-itl");
        System.out.println("itl-inherited: " + onVirtual(Thread.ofVirtual(), ITL::get));
        System.out.println("itl-not-inherited: "
            + onVirtual(Thread.ofVirtual().inheritInheritableThreadLocals(false), ITL::get));
        System.out.println("itl-platform-not-inherited: "
            + onVirtual(Thread.ofPlatform().inheritInheritableThreadLocals(false), ITL::get));

        COUNTED.get();
        COUNTED.get();
        onVirtual(Thread.ofVirtual(), () -> String.valueOf(COUNTED.get() + COUNTED.get()));
        onVirtual(Thread.ofVirtual(), () -> String.valueOf(COUNTED.get()));
        System.out.println("initial-calls: " + INITIALS.get());

        // The VirtualThreads group while a virtual thread is parked in it.
        AtomicReference<ThreadGroup> group = new AtomicReference<>();
        CountDownLatch release = new CountDownLatch(1);
        Thread parked = Thread.ofVirtual().start(() -> {
            group.set(Thread.currentThread().getThreadGroup());
            try {
                release.await();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        while (parked.getState() != Thread.State.WAITING) {
            Thread.sleep(1);
        }
        ThreadGroup vg = group.get();
        System.out.println("group-activeCount: " + (vg == null ? -1 : vg.activeCount()));
        System.out.println("group-enumerate: "
            + (vg == null ? -1 : vg.enumerate(new Thread[16])));
        release.countDown();
        parked.join();
        ThreadGroup after = parked.getThreadGroup();
        System.out.println("terminated-group: " + (after == null ? "null" : after.getName()));
        System.out.println("done");
    }
}
