// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L6 -- ThreadMXBean deadlock detection
// over monitors AND ownable synchronizers, a parked thread's lock owner, and
// the thread-CPU-time enable switch.
//
// HotSpot 25 (`ThreadService::find_deadlocks_at_safepoint`, `ThreadSnapshot`,
// `jmm_SetBoolAttribute`):
// * `findMonitorDeadlockedThreads()` follows monitor-entry edges only;
//   `findDeadlockedThreads()` also follows a parked thread's `parkBlocker`
//   when it is an `AbstractOwnableSynchronizer`, to its
//   `exclusiveOwnerThread`. Three cycles below: two monitors (mA*), two
//   `ReentrantLock`s (rB*), and one of each (xC*) -- the monitor detector
//   finds only the first, the full one all three.
// * a thread parked in `ReentrantLock.lock()` reports the lock's `Sync` as
//   its lock and the holder as `getLockOwnerName()` / `getLockOwnerId()`.
// * `setThreadCpuTimeEnabled(false)` makes `isThreadCpuTimeEnabled()` false
//   and every CPU-time getter -1; re-enabling restores them.
//
// Before wave 84 CratonVM's `findDeadlockedThreads()` was the monitor-only
// walk (rB* and xC* missing), a parked thread's owner was -1 / null, and
// `sun.management.ThreadImpl.setThreadCpuTimeEnabled0` was a no-op (enabled
// stayed true, times kept coming).
//
// Run, each mode:
//   cratonvm L6W84DeadlockDetection
//   cratonvm --compatible L6W84DeadlockDetection
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// cycles formed: true
// findMonitorDeadlockedThreads: [mA1, mA2]
// findDeadlockedThreads: [mA1, mA2, rB1, rB2, xC1, xC2]
// mA1: state=BLOCKED lock=java.lang.Object owner=mA2 ownerIdMatches=true lockedMonitors=1 lockedSynchronizers=[]
// rB1: state=WAITING lock=java.util.concurrent.locks.ReentrantLock$NonfairSync owner=rB2 ownerIdMatches=true lockedMonitors=0 lockedSynchronizers=[java.util.concurrent.locks.ReentrantLock$NonfairSync]
// xC2: state=BLOCKED lock=java.lang.Object owner=xC1 ownerIdMatches=true lockedMonitors=0 lockedSynchronizers=[java.util.concurrent.locks.ReentrantLock$NonfairSync]
// cpu supported: true
// cpu enabled at start: true
// after disable: enabled=false current=-1 main=-1
// after enable: enabled=true current>0=true main>0=true
// unknown id: -1
// HOTSPOT_EXPECTED_END

import java.lang.management.LockInfo;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.ReentrantLock;

public class L6W84DeadlockDetection {
    static final ThreadMXBean BEAN = ManagementFactory.getThreadMXBean();

    static void await(CountDownLatch latch) {
        try {
            latch.await();
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    static Thread daemon(String name, Runnable r) {
        Thread t = new Thread(r, name);
        t.setDaemon(true);
        t.start();
        return t;
    }

    static List<String> names(long[] ids) {
        List<String> out = new ArrayList<>();
        if (ids == null) {
            return out;
        }
        for (ThreadInfo info : BEAN.getThreadInfo(ids)) {
            if (info != null) {
                out.add(info.getThreadName());
            }
        }
        Collections.sort(out);
        return out;
    }

    static String describe(Thread t, Thread owner) {
        ThreadInfo info = BEAN.getThreadInfo(new long[] {t.threadId()}, true, true)[0];
        LockInfo lock = info.getLockInfo();
        List<String> syncs = new ArrayList<>();
        for (LockInfo s : info.getLockedSynchronizers()) {
            syncs.add(s.getClassName());
        }
        return t.getName() + ": state=" + info.getThreadState()
                + " lock=" + (lock == null ? "null" : lock.getClassName())
                + " owner=" + info.getLockOwnerName()
                + " ownerIdMatches=" + (info.getLockOwnerId() == owner.threadId())
                + " lockedMonitors=" + info.getLockedMonitors().length
                + " lockedSynchronizers=" + syncs;
    }

    public static void main(String[] args) throws Exception {
        // A: two monitors.
        Object a = new Object();
        Object b = new Object();
        CountDownLatch gateA = new CountDownLatch(2);
        Thread mA1 = daemon("mA1", () -> {
            synchronized (a) {
                gateA.countDown();
                await(gateA);
                synchronized (b) {
                }
            }
        });
        Thread mA2 = daemon("mA2", () -> {
            synchronized (b) {
                gateA.countDown();
                await(gateA);
                synchronized (a) {
                }
            }
        });
        // B: two ReentrantLocks.
        ReentrantLock l1 = new ReentrantLock();
        ReentrantLock l2 = new ReentrantLock();
        CountDownLatch gateB = new CountDownLatch(2);
        Thread rB1 = daemon("rB1", () -> {
            l1.lock();
            gateB.countDown();
            await(gateB);
            l2.lock();
        });
        Thread rB2 = daemon("rB2", () -> {
            l2.lock();
            gateB.countDown();
            await(gateB);
            l1.lock();
        });
        // C: one monitor, one ReentrantLock.
        Object m = new Object();
        ReentrantLock rl = new ReentrantLock();
        CountDownLatch gateC = new CountDownLatch(2);
        Thread xC1 = daemon("xC1", () -> {
            synchronized (m) {
                gateC.countDown();
                await(gateC);
                rl.lock();
            }
        });
        Thread xC2 = daemon("xC2", () -> {
            rl.lock();
            gateC.countDown();
            await(gateC);
            synchronized (m) {
            }
        });

        boolean formed = false;
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (System.nanoTime() < deadline) {
            if (names(BEAN.findDeadlockedThreads()).size() == 6
                    && names(BEAN.findMonitorDeadlockedThreads()).size() >= 2) {
                formed = true;
                break;
            }
            Thread.sleep(20);
        }
        System.out.println("cycles formed: " + formed);
        System.out.println("findMonitorDeadlockedThreads: " + names(BEAN.findMonitorDeadlockedThreads()));
        System.out.println("findDeadlockedThreads: " + names(BEAN.findDeadlockedThreads()));
        System.out.println(describe(mA1, mA2));
        System.out.println(describe(rB1, rB2));
        System.out.println(describe(xC2, xC1));

        long main = Thread.currentThread().threadId();
        System.out.println("cpu supported: " + BEAN.isThreadCpuTimeSupported());
        System.out.println("cpu enabled at start: " + BEAN.isThreadCpuTimeEnabled());
        BEAN.setThreadCpuTimeEnabled(false);
        System.out.println("after disable: enabled=" + BEAN.isThreadCpuTimeEnabled()
                + " current=" + BEAN.getCurrentThreadCpuTime()
                + " main=" + BEAN.getThreadCpuTime(main));
        BEAN.setThreadCpuTimeEnabled(true);
        long spin = 0;
        for (int i = 0; i < 2_000_000; i++) {
            spin += i ^ (spin >>> 3);
        }
        System.out.println("after enable: enabled=" + BEAN.isThreadCpuTimeEnabled()
                + " current>0=" + (BEAN.getCurrentThreadCpuTime() > 0 || spin == 42)
                + " main>0=" + (BEAN.getThreadCpuTime(main) > 0));
        System.out.println("unknown id: " + BEAN.getThreadCpuTime(Long.MAX_VALUE));
        System.exit(0);
    }
}
