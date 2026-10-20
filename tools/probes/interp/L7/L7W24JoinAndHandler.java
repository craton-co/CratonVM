// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L7: `Thread.join()` with two concurrent
// joiners of a worker that runs longer than a second, an interrupted
// `join()`, and `getUncaughtExceptionHandler()` of a terminated thread.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W24JoinAndHandler
//
// HotSpot 25 prints exactly:
//     joiner A saw done=true
//     joiner B saw done=true
//     interrupted join java.lang.InterruptedException flag=false
//     join of a dead thread while interrupted returned flag=true
//     live handler=own
//     live default handler=group
//     dead handler=null
//     dead default handler=null
//
// CratonVM before wave 24 (read from the code): the `joiner` and `interrupted
// join` rows are right in the default (`--jdk-only`) and `--compatible` modes,
// where `Thread.join` is real bytecode (`wait` on the thread object); with the
// synthetic-JDK natives (`native_thread_join` -> `ThreadRegistry::join`) the
// second joiner returned after about 1 s with `done=false` and the
// interrupted join never threw. `--compatible`: `dead handler` printed the
// handler and `dead default handler` the thread group (the registered
// `getUncaughtExceptionHandler` native did not apply `isTerminated()`).
public class L7W24JoinAndHandler {
    static volatile boolean done;

    public static void main(String[] args) throws Exception {
        Thread worker = new Thread(() -> {
            try {
                Thread.sleep(1_500);
            } catch (InterruptedException e) {
            }
            done = true;
        });
        worker.start();
        String[] seen = new String[2];
        Thread a = new Thread(() -> {
            try {
                worker.join();
                seen[0] = "done=" + done;
            } catch (InterruptedException e) {
                seen[0] = e.toString();
            }
        });
        Thread b = new Thread(() -> {
            try {
                worker.join();
                seen[1] = "done=" + done;
            } catch (InterruptedException e) {
                seen[1] = e.toString();
            }
        });
        a.start();
        b.start();
        a.join();
        b.join();
        System.out.println("joiner A saw " + seen[0]);
        System.out.println("joiner B saw " + seen[1]);

        Thread forever = new Thread(() -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
            }
        });
        forever.setDaemon(true);
        forever.start();
        String[] r = new String[1];
        Thread joiner = new Thread(() -> {
            try {
                forever.join();
                r[0] = "returned";
            } catch (InterruptedException e) {
                r[0] = e.getClass().getName();
            }
            r[0] += " flag=" + Thread.currentThread().isInterrupted();
        });
        joiner.start();
        long end = System.nanoTime() + 5_000_000_000L;
        while (joiner.getState() != Thread.State.WAITING && System.nanoTime() < end) {
            Thread.sleep(2);
        }
        Thread.sleep(20);
        joiner.interrupt();
        joiner.join(10_000);
        System.out.println("interrupted join " + r[0]);

        Thread.currentThread().interrupt();
        String o;
        try {
            worker.join();
            o = "returned";
        } catch (InterruptedException e) {
            o = e.getClass().getName();
        }
        System.out.println("join of a dead thread while interrupted " + o
                + " flag=" + Thread.interrupted());

        Thread.UncaughtExceptionHandler own = (t, e) -> { };
        Thread h1 = new Thread(() -> { });
        h1.setUncaughtExceptionHandler(own);
        Thread h2 = new Thread(() -> { });
        System.out.println("live handler=" + describe(h1.getUncaughtExceptionHandler(), own, h1));
        System.out.println("live default handler="
                + describe(h2.getUncaughtExceptionHandler(), own, h2));
        h1.start();
        h2.start();
        h1.join();
        h2.join();
        System.out.println("dead handler=" + describe(h1.getUncaughtExceptionHandler(), own, h1));
        System.out.println("dead default handler="
                + describe(h2.getUncaughtExceptionHandler(), own, h2));
    }

    static String describe(Thread.UncaughtExceptionHandler h, Thread.UncaughtExceptionHandler own,
            Thread t) {
        if (h == null) {
            return "null";
        }
        if (h == own) {
            return "own";
        }
        if (h instanceof ThreadGroup) {
            return "group";
        }
        return h.getClass().getName();
    }
}
