// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L6 -- VM lifecycle shapes, one mode
// per run (the exit code is part of the answer). Compare stdout AND the exit
// status with HotSpot 25, in both modes (`cratonvm` and `cratonvm
// --compatible`); stderr carries stack traces and is compared by its first
// line only.
//
//   for m in hooks halt-in-hook exit-in-hook daemon exit-other uncaught-main \
//            uncaught-other misc; do
//     java -cp out L6W101ShutdownShapes $m; echo "rc=$?"
//   done
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// == hooks (rc=0; stderr first line: Exception in thread "hook-c" java.lang.RuntimeException: boom)
//    (wave 102: the hooks are sequenced -- hook-c's whole trace, then hook-b's
//    lines, then hook-a's -- so a 2>&1 capture has one order; stdout unchanged)
// main done
// add during shutdown: java.lang.IllegalStateException: Shutdown in progress
// remove during shutdown: java.lang.IllegalStateException: Shutdown in progress
// hook-a saw hook-b running: true
// == halt-in-hook (rc=7)
// main done
// hook halting
// == exit-in-hook (rc=2)
// main exiting
// second exit blocks: BLOCKED
// == daemon (rc=0)
// main done
// == exit-other (rc=4)
// exiting from other
// hook ran on exit from other
// == uncaught-main (rc=1; stderr first line: Exception in thread "main" java.lang.IllegalStateException: main fails)
// main throwing
// == uncaught-other (rc=0; stderr first line: Exception in thread "worker" java.lang.IllegalStateException: worker fails)
// worker joined
// main done
// == misc (rc=0)
// stop: java.lang.UnsupportedOperationException
// start twice: java.lang.IllegalThreadStateException
// setDaemon on started: java.lang.IllegalThreadStateException
// Thread.suspend declared: false
// Thread.resume declared: false
// processors >= 1: true
// free <= total <= max: true
// HOTSPOT_EXPECTED_END
// (The two "declared" rows assume JDK 23's removal of Thread.suspend/resume,
// JDK-8320532; check them against HotSpot 25 before trusting a diff there.)

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public class L6W101ShutdownShapes {
    static volatile boolean hookBRunning;

    public static void main(String[] args) throws Exception {
        switch (args.length == 0 ? "hooks" : args[0]) {
            case "hooks" -> hooks();
            case "halt-in-hook" -> haltInHook();
            case "exit-in-hook" -> exitInHook();
            case "daemon" -> daemon();
            case "exit-other" -> exitOther();
            case "uncaught-main" -> {
                System.out.println("main throwing");
                throw new IllegalStateException("main fails");
            }
            case "uncaught-other" -> uncaughtOther();
            case "misc" -> misc();
            default -> System.out.println("unknown mode");
        }
    }

    static void hooks() {
        CountDownLatch bStarted = new CountDownLatch(1);
        Thread a = new Thread(() -> {
            boolean saw = false;
            try {
                saw = bStarted.await(5, TimeUnit.SECONDS);
            } catch (InterruptedException e) {
                // reported as false
            }
            System.out.println("hook-a saw hook-b running: " + saw);
        }, "hook-a");
        Thread c = new Thread(() -> {
            throw new RuntimeException("boom");
        }, "hook-c");
        Thread b = new Thread(() -> {
            // Wave 102 (lane L6): the hooks run concurrently on both VMs, so
            // hook-b prints only once hook-c has ended -- its uncaught trace
            // (the default handler, printed on hook-c before it terminates) is
            // complete on stderr before any line below; hook-a prints after
            // hook-b (the latch). One fixed order on every VM and every run.
            // The hooks are started one after another in an unspecified order
            // (`ApplicationShutdownHooks.runHooks`; `--compatible`'s runner:
            // registration order, b before c), so hook-c may still be NEW
            // here, and `join` of an unstarted thread returns at once. Wait
            // for its start first (bounded), then for its end.
            try {
                long end = System.currentTimeMillis() + 5_000;
                while (c.getState() == Thread.State.NEW && System.currentTimeMillis() < end) {
                    Thread.sleep(1);
                }
                c.join(5_000);
            } catch (InterruptedException e) {
                // reported by the order of the lines
            }
            try {
                Runtime.getRuntime().addShutdownHook(new Thread(() -> { }));
                System.out.println("add during shutdown: accepted");
            } catch (Throwable t) {
                System.out.println("add during shutdown: " + t);
            }
            try {
                Runtime.getRuntime().removeShutdownHook(a);
                System.out.println("remove during shutdown: accepted");
            } catch (Throwable t) {
                System.out.println("remove during shutdown: " + t);
            }
            // Still running: hook-a, started by another hook, waits for this.
            bStarted.countDown();
        }, "hook-b");
        Runtime.getRuntime().addShutdownHook(b);
        Runtime.getRuntime().addShutdownHook(c);
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            try {
                a.start();
                a.join();
            } catch (InterruptedException e) {
                // nothing
            }
        }, "hook-a-runner"));
        System.out.println("main done");
    }

    static void haltInHook() {
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            System.out.println("hook halting");
            System.out.flush();
            Runtime.getRuntime().halt(7);
        }, "halter"));
        System.out.println("main done");
    }

    static void exitInHook() {
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            Thread second = new Thread(() -> System.exit(5), "second-exit");
            second.setDaemon(true);
            second.start();
            for (int i = 0; i < 2000 && second.getState() != Thread.State.BLOCKED; i++) {
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
            System.out.println("second exit blocks: " + second.getState());
        }, "exit-hook"));
        System.out.println("main exiting");
        System.exit(2);
    }

    static void daemon() {
        Thread d = new Thread(() -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
                return;
            }
            System.out.println("daemon must not print");
        }, "sleeper");
        d.setDaemon(true);
        d.start();
        System.out.println("main done");
    }

    static void exitOther() throws InterruptedException {
        Runtime.getRuntime().addShutdownHook(new Thread(
                () -> System.out.println("hook ran on exit from other"), "hook"));
        Thread t = new Thread(() -> {
            System.out.println("exiting from other");
            System.exit(4);
        }, "exiter");
        t.start();
        Thread.sleep(60_000);
        System.out.println("main must not finish");
    }

    static void uncaughtOther() throws InterruptedException {
        Thread t = new Thread(() -> {
            throw new IllegalStateException("worker fails");
        }, "worker");
        t.start();
        t.join();
        System.out.println("worker joined");
        System.out.println("main done");
    }

    static String declared(String name) {
        try {
            Thread.class.getDeclaredMethod(name);
            return "true";
        } catch (NoSuchMethodException e) {
            return "false";
        }
    }

    @SuppressWarnings({"deprecation", "removal"})
    static void misc() throws Exception {
        Thread t = new Thread(() -> { }, "t");
        try {
            t.stop();
            System.out.println("stop: returned");
        } catch (Throwable e) {
            System.out.println("stop: " + e);
        }
        t.start();
        t.join();
        try {
            t.start();
            System.out.println("start twice: accepted");
        } catch (Throwable e) {
            System.out.println("start twice: " + e);
        }
        Thread live = new Thread(() -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
                // ended by the interrupt below
            }
        }, "live");
        live.start();
        try {
            live.setDaemon(true);
            System.out.println("setDaemon on started: accepted");
        } catch (Throwable e) {
            System.out.println("setDaemon on started: " + e);
        }
        live.interrupt();
        live.join();
        System.out.println("Thread.suspend declared: " + declared("suspend"));
        System.out.println("Thread.resume declared: " + declared("resume"));
        Runtime rt = Runtime.getRuntime();
        System.out.println("processors >= 1: " + (rt.availableProcessors() >= 1));
        long free = rt.freeMemory();
        long total = rt.totalMemory();
        long max = rt.maxMemory();
        System.out.println("free <= total <= max: " + (free >= 0 && free <= total && total <= max));
    }
}
