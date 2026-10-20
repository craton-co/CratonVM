// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L7: `Thread.getState()` of a thread in
// `Object.wait`, read by the thread that takes the monitor the moment the
// waiter released it. HotSpot sets the waiter's state before the release
// (`ObjectMonitor::wait`: `JavaThreadInObjectWaitState`, then `exit`), so
// whoever acquires the monitor next always reads WAITING / TIMED_WAITING.
//
// CratonVM (base d226deb8fd): `holder.threadStatus` -- which the JDK's
// `threadState()` reads since SBR round 2 retired the `getState` native -- was
// written WAITING only by the wait prologue's deposit, which runs AFTER the
// release and, under the lazy park (`CRATONVM_MONITOR_LAZY_PARK_US`, 2 ms by
// default), only after the running phase ends: for up to 2 ms after the
// release the next owner read RUNNABLE. Fixed by publishing the state word
// before the release (`monitor_wait_with_state`).
//
//   untimed  wait()       read under the monitor right after the release
//   timed    wait(600000) the same
//   both rows repeated 3 times (cold first, then warm)
//
// Expected (HotSpot 25.0.3, `java L7W54WaitStateAtRelease`, default flags and
// -Xint, three runs each):
//   untimed WAITING
//   timed TIMED_WAITING
//   untimed WAITING
//   timed TIMED_WAITING
//   untimed WAITING
//   timed TIMED_WAITING

public class L7W54WaitStateAtRelease {
    static volatile boolean ready;

    static Thread.State once(long timeout) throws Exception {
        Object m = new Object();
        ready = false;
        Thread a = new Thread(() -> {
            synchronized (m) {
                ready = true;
                try {
                    if (timeout > 0) m.wait(timeout); else m.wait();
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            }
        });
        a.start();
        while (!ready) Thread.onSpinWait();
        Thread.State s;
        synchronized (m) {
            // The waiter released `m` inside `wait`: it is waiting now.
            s = a.getState();
            m.notify();
        }
        a.join();
        return s;
    }

    public static void main(String[] args) throws Exception {
        for (int r = 0; r < 3; r++) {
            System.out.println("untimed " + once(0));
            System.out.println("timed " + once(600_000));
        }
    }
}
