// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L6 -- the main thread after `main`
// returns, while the VM waits for a non-daemon thread
// (`CRATONVM_MAIN_THREAD_ENDS_BEFORE_DESTROY`, default on, `=0` kill switch).
//
// HotSpot 25: the launcher's `JavaMain` `LEAVE()` DETACHES the main thread
// (`JavaThread::exit`: `Thread.exit()`, TERMINATED, `eetop` 0, `notifyAll`)
// before `DestroyJavaVM` waits for the non-daemon threads. So a non-daemon
// thread joining `main` returns at once, and sees it TERMINATED, not alive,
// with no thread group. CratonVM kept the main thread alive through the
// wait: the join below timed out after 10 s (an untimed `join()` never
// returned and the process never exited).
//
// Run: java -cp out L6W101MainJoinAfterMainReturns
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// main returning
// joined main in under 5 s: true
// main alive: false
// main state: TERMINATED
// main group: null
// HOTSPOT_EXPECTED_END

public class L6W101MainJoinAfterMainReturns {
    public static void main(String[] args) throws Exception {
        Thread main = Thread.currentThread();
        Thread watcher = new Thread(() -> {
            long t0 = System.nanoTime();
            try {
                main.join(10_000);
            } catch (InterruptedException e) {
                return;
            }
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("joined main in under 5 s: " + (ms < 5_000));
            System.out.println("main alive: " + main.isAlive());
            System.out.println("main state: " + main.getState());
            System.out.println("main group: " + main.getThreadGroup());
        }, "watcher");
        watcher.setDaemon(false);
        watcher.start();
        Thread.sleep(100);
        System.out.println("main returning");
    }
}
