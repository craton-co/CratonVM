// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L6 -- the end of main with live threads,
// against HotSpot 25 (DestroyJavaVM): main returns while a non-daemon platform
// thread still works; the VM waits for it (its lines print after "main
// returns"), a daemon platform thread and a virtual thread (always daemon)
// still sleeping are abandoned (their lines never print), the shutdown hook
// runs after the last non-daemon thread, and the exit status is 0. A
// non-daemon thread started BY the non-daemon worker is waited for too.
//
// HOTSPOT_EXPECTED_BEGIN
// main returns
// worker done
// grandchild done
// hook runs; worker alive=false
// HOTSPOT_EXPECTED_END
public class L6W98MainExitWaitsNonDaemon {
    static volatile Thread worker;

    static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            System.out.println("interrupted " + Thread.currentThread().getName());
        }
    }

    public static void main(String[] args) {
        Runtime.getRuntime().addShutdownHook(new Thread(() ->
                System.out.println("hook runs; worker alive=" + worker.isAlive())));
        Thread daemon = new Thread(() -> {
            sleep(60_000);
            System.out.println("daemon must not print");
        }, "daemon");
        daemon.setDaemon(true);
        daemon.start();
        Thread.ofVirtual().start(() -> {
            sleep(60_000);
            System.out.println("virtual must not print");
        });
        worker = new Thread(() -> {
            sleep(300);
            Thread grandchild = new Thread(() -> {
                sleep(300);
                System.out.println("grandchild done");
            }, "grandchild");
            grandchild.start();
            System.out.println("worker done");
        }, "worker");
        worker.start();
        System.out.println("main returns");
    }
}
