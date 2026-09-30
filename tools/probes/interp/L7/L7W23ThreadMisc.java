// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 23, lane L7 (bug hunt): thread-control APIs
// JDK 25 removed or constrained, the uncaught-exception handler order, and
// `Thread.getStackTrace()` of another thread.
//
//   stop                  UnsupportedOperationException (JDK 20+), thread
//                         keeps running (suspend / resume are gone from
//                         JDK 25's Thread)
//   ueh                   which handler sees an uncaught exception: the
//                         thread's own, else its group's `uncaughtException`
//                         (which delegates to the default handler), else the
//                         default handler; the thread is TERMINATED after it
//   trace                 the top frames of a thread parked in a known method
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W23ThreadMisc
//
// HotSpot 25 prints exactly:
//     stop java.lang.UnsupportedOperationException: null alive=true
//     ueh own: own saw t1 boom1
//     ueh group: group saw t2 boom2
//     ueh group: default saw t2 boom2
//     ueh default: default saw t3 boom3
//     ueh state TERMINATED
//     ueh handlerThrows: done
//     trace parkedHere <- L7W23ThreadMisc.lambda$trace
//     trace self getStackTrace <- L7W23ThreadMisc.main
// and on stderr, for t4, `Exception: java.lang.IllegalStateException thrown
// from the UncaughtExceptionHandler in thread "t4"` (preceded by a blank line).
import java.util.concurrent.CountDownLatch;

public class L7W23ThreadMisc {
    static final StringBuilder LOG = new StringBuilder();

    static synchronized void log(String s) {
        LOG.append(s).append('\n');
    }

    static String outcome(Runnable r) {
        try {
            r.run();
            return "returned";
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    @SuppressWarnings({"deprecation", "removal"})
    static void control() throws Exception {
        CountDownLatch stop = new CountDownLatch(1);
        Thread t = new Thread(() -> {
            try {
                stop.await();
            } catch (InterruptedException e) {
            }
        });
        t.start();
        System.out.println("stop " + outcome(t::stop) + " alive=" + t.isAlive());
        stop.countDown();
        t.join();
    }

    static void ueh() throws Exception {
        Thread.UncaughtExceptionHandler dflt = (th, e) -> log("default saw " + th.getName() + " " + e.getMessage());
        Thread.setDefaultUncaughtExceptionHandler(dflt);

        Thread t1 = new Thread(() -> {
            throw new RuntimeException("boom1");
        }, "t1");
        t1.setUncaughtExceptionHandler((th, e) -> log("own saw " + th.getName() + " " + e.getMessage()));
        t1.start();
        t1.join();
        System.out.print("ueh own: " + LOG);
        LOG.setLength(0);

        ThreadGroup g = new ThreadGroup("g") {
            @Override
            public void uncaughtException(Thread th, Throwable e) {
                log("group saw " + th.getName() + " " + e.getMessage());
                super.uncaughtException(th, e);
            }
        };
        Thread t2 = new Thread(g, () -> {
            throw new RuntimeException("boom2");
        }, "t2");
        t2.start();
        t2.join();
        for (String line : LOG.toString().split("\n")) {
            System.out.println("ueh group: " + line);
        }
        LOG.setLength(0);

        Thread t3 = new Thread(() -> {
            throw new RuntimeException("boom3");
        }, "t3");
        t3.start();
        t3.join();
        System.out.print("ueh default: " + LOG);
        LOG.setLength(0);
        System.out.println("ueh state " + t3.getState());

        // A handler that itself throws: the VM ignores the handler's exception.
        Thread t4 = new Thread(() -> {
            throw new RuntimeException("boom4");
        }, "t4");
        t4.setUncaughtExceptionHandler((th, e) -> {
            throw new IllegalStateException("handler");
        });
        t4.start();
        t4.join();
        System.out.println("ueh handlerThrows: done");
        Thread.setDefaultUncaughtExceptionHandler(null);
    }

    static void parkedHere(CountDownLatch in, CountDownLatch out) throws InterruptedException {
        in.countDown();
        out.await();
    }

    static void trace() throws Exception {
        CountDownLatch in = new CountDownLatch(1);
        CountDownLatch out = new CountDownLatch(1);
        Thread t = new Thread(() -> {
            try {
                parkedHere(in, out);
            } catch (InterruptedException e) {
            }
        });
        t.start();
        in.await();
        while (t.getState() != Thread.State.WAITING) {
            Thread.sleep(2);
        }
        StackTraceElement[] st = t.getStackTrace();
        // The frames below parkedHere are JDK internals (CountDownLatch/AQS);
        // report the first frame of this class and the one after it.
        String line = "missing";
        for (int i = 0; i < st.length; i++) {
            if (st[i].getClassName().equals("L7W23ThreadMisc")) {
                String next = i + 1 < st.length ? st[i + 1].getClassName() + "." + st[i + 1].getMethodName() : "-";
                line = st[i].getMethodName() + " <- " + next.replaceAll("\\$\\d+$", "").replaceAll("lambda\\$trace\\$\\d+", "lambda");
                break;
            }
        }
        System.out.println("trace " + line);
        out.countDown();
        t.join();

        StackTraceElement[] self = Thread.currentThread().getStackTrace();
        System.out.println("trace self " + self[0].getMethodName() + " <- " + self[1].getClassName() + "." + self[2].getMethodName());
    }

    public static void main(String[] args) throws Exception {
        control();
        ueh();
        trace();
    }
}
