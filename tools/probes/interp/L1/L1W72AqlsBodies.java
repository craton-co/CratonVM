// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L1:
//
// * `CRATONVM_COMPAT_JDK_AQLS` (the `JdkAqls` HotSpot-alignment row, default
//   on; docs/internal/fixed-bugs/interpreter-i71-L1-compatible-aqls-state-lives-in-a-process-global-side-table-FIXED-20261105.md):
//   `--compatible` over a real JDK no longer registers the three
//   `AbstractQueuedLongSynchronizer` state stand-ins (`getState`, `setState`,
//   `compareAndSetState` over a process-global side table that never wrote the
//   real `state` field), so the JDK 25 bodies run on the field, as `--jdk-only`
//   already did.
//
// The rows a stand-in run differs on (expected from code reading, with
// `CRATONVM_COMPAT_JDK_AQLS=0`): both serialization rows (the stream carries
// the never-written field, so the copy reads 0 / and the original's write was
// to the table, not the field). Every other row is the JDK's acquire / release
// / condition bytecode in both arms, over whichever state accessors are live.
//
// Run in both modes: `cratonvm L1W72AqlsBodies` (jdk-only) and
// `cratonvm --compatible L1W72AqlsBodies`; both must print the block below.
// `=0` arm: `CRATONVM_COMPAT_JDK_AQLS=0 cratonvm --compatible L1W72AqlsBodies`.
//
// The expected block is DERIVED from the JDK 25 sources
// (AbstractQueuedLongSynchronizer / AbstractOwnableSynchronizer /
// ObjectOutputStream), not yet captured from the host oracle: refresh it from
// the host's HotSpot run before reading a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// new: getState=0 toString=[State = 0, empty queue]
// setState(2^40): getState=1099511627776
// cas(2^40 -> 5): true then false getState=5
// cas(MIN -> MAX): true getState=9223372036854775807
// serialized at 42: copy getState=42 toString=[State = 42, empty queue]
// serialized after cas to 7: copy getState=7
// acquire(1): getState=1 held=true
// release(1): getState=0 held=false
// tryAcquire while held elsewhere: false
// parked acquire: WAITING blocker=L1W72AqlsBodies$LongLock toString=[State = 1, nonempty queue]
// parked acquire released: acquired=true
// condition awaitNanos(1 ms): timed-out=true getState=1 held=true
// contended: 2 x 20000 increments=40000 getState=0
// done
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.AbstractQueuedLongSynchronizer;
import java.util.concurrent.locks.LockSupport;

public class L1W72AqlsBodies {
    /** A one-holder lock whose state is the long the holder acquired with. */
    static final class LongLock extends AbstractQueuedLongSynchronizer {
        private static final long serialVersionUID = 1L;

        @Override
        protected boolean tryAcquire(long arg) {
            if (compareAndSetState(0, arg)) {
                setExclusiveOwnerThread(Thread.currentThread());
                return true;
            }
            return false;
        }

        @Override
        protected boolean tryRelease(long arg) {
            if (getExclusiveOwnerThread() != Thread.currentThread()) {
                throw new IllegalMonitorStateException();
            }
            setExclusiveOwnerThread(null);
            setState(0);
            return true;
        }

        @Override
        protected boolean isHeldExclusively() {
            return getExclusiveOwnerThread() == Thread.currentThread();
        }

        long state() {
            return getState();
        }

        void state(long v) {
            setState(v);
        }

        boolean cas(long expect, long update) {
            return compareAndSetState(expect, update);
        }

        ConditionObject newCondition() {
            return new ConditionObject();
        }
    }

    static String suffix(Object o) {
        String s = String.valueOf(o);
        int at = s.indexOf('[');
        return at < 0 ? s : s.substring(at);
    }

    static LongLock roundTrip(LongLock l) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(l);
        }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            return (LongLock) in.readObject();
        }
    }

    static boolean waiting(Thread.State s) {
        return s == Thread.State.WAITING || s == Thread.State.TIMED_WAITING;
    }

    public static void main(String[] args) throws Exception {
        LongLock l = new LongLock();
        System.out.println("new: getState=" + l.state() + " toString=" + suffix(l));

        l.state(1L << 40);
        System.out.println("setState(2^40): getState=" + l.state());

        boolean first = l.cas(1L << 40, 5);
        boolean second = l.cas(1L << 40, 6);
        System.out.println("cas(2^40 -> 5): " + first + " then " + second + " getState=" + l.state());

        l.state(Long.MIN_VALUE);
        boolean wide = l.cas(Long.MIN_VALUE, Long.MAX_VALUE);
        System.out.println("cas(MIN -> MAX): " + wide + " getState=" + l.state());

        LongLock s = new LongLock();
        s.state(42);
        LongLock copy = roundTrip(s);
        System.out.println("serialized at 42: copy getState=" + copy.state() + " toString=" + suffix(copy));

        LongLock c = new LongLock();
        c.cas(0, 7);
        System.out.println("serialized after cas to 7: copy getState=" + roundTrip(c).state());

        LongLock lock = new LongLock();
        lock.acquire(1);
        System.out.println("acquire(1): getState=" + lock.state() + " held=" + lock.isHeldExclusively());
        lock.release(1);
        System.out.println("release(1): getState=" + lock.state() + " held=" + lock.isHeldExclusively());

        lock.acquire(1);
        boolean[] other = {true};
        Thread probe = new Thread(() -> other[0] = lock.tryAcquire(1), "try-acquire");
        probe.start();
        probe.join();
        System.out.println("tryAcquire while held elsewhere: " + other[0]);

        boolean[] acquired = {false};
        Thread waiter = new Thread(() -> {
            lock.acquire(1);
            acquired[0] = lock.isHeldExclusively();
            lock.release(1);
        }, "parked-acquire");
        waiter.setDaemon(true);
        waiter.start();
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!(waiting(waiter.getState()) && lock.hasQueuedThreads()) && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(50);
        Object blocker = LockSupport.getBlocker(waiter);
        System.out.println("parked acquire: " + waiter.getState()
                + " blocker=" + (blocker == null ? "null" : blocker.getClass().getName())
                + " toString=" + suffix(lock));
        lock.release(1);
        waiter.join(5_000);
        System.out.println("parked acquire released: acquired=" + acquired[0]);

        LongLock cl = new LongLock();
        AbstractQueuedLongSynchronizer.ConditionObject cond = cl.newCondition();
        cl.acquire(1);
        long left = cond.awaitNanos(TimeUnit.MILLISECONDS.toNanos(1));
        System.out.println("condition awaitNanos(1 ms): timed-out=" + (left <= 0)
                + " getState=" + cl.state() + " held=" + cl.isHeldExclusively());
        cl.release(1);

        LongLock counter = new LongLock();
        long[] count = {0};
        Runnable body = () -> {
            for (int i = 0; i < 20_000; i++) {
                counter.acquire(1);
                count[0]++;
                counter.release(1);
            }
        };
        Thread a = new Thread(body, "contended-a");
        Thread b = new Thread(body, "contended-b");
        a.start();
        b.start();
        a.join();
        b.join();
        System.out.println("contended: 2 x 20000 increments=" + count[0] + " getState=" + counter.state());
        System.out.println("done");
    }
}
