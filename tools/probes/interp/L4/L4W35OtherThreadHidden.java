// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 35 (orchestrator): does another thread's
// `Thread.getStackTrace()` (and `Thread.getAllStackTraces()`) show hidden
// frames? (docs/internal/fixed-bugs/interpreter-L4-cross-loader-type-checks-and-trace-shapes-FIXED-20261001.md,
// item 4.) A platform thread parks inside `ScopedValue.where(k, v).run(r)`, so
// the `@Hidden` `ScopedValue$Carrier.runWith` is on its stack; `main` reads the
// trace.
//
// HotSpot leaves the hidden frame out of `getStackTrace()` (its
// `asyncGetStackTrace` walk skips hidden frames) but keeps it in
// `getAllStackTraces()` (`dumpThreads`). Before wave 35 CratonVM showed it in
// both; `--compatible` still does.
//
// Run: javac -d out L4W35OtherThreadHidden.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W35OtherThreadHidden
//
// Expected HotSpot 25 output (default and -Xint):
//   getStackTrace sees Carrier.run: true
//   getStackTrace shows runWith: false
//   getAllStackTraces sees Carrier.run: true
//   getAllStackTraces shows runWith: true
import java.util.concurrent.CountDownLatch;

public class L4W35OtherThreadHidden {
    static final ScopedValue<String> KEY = ScopedValue.newInstance();

    static boolean shows(StackTraceElement[] trace) {
        for (StackTraceElement e : trace) {
            if (e.getClassName().equals("java.lang.ScopedValue$Carrier") && e.getMethodName().equals("runWith")) {
                return true;
            }
        }
        return false;
    }

    static boolean carrierRun(StackTraceElement[] trace) {
        for (StackTraceElement e : trace) {
            if (e.getClassName().equals("java.lang.ScopedValue$Carrier") && e.getMethodName().equals("run")) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) throws Exception {
        CountDownLatch parked = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        Thread t = new Thread(() -> ScopedValue.where(KEY, "v").run(() -> {
            parked.countDown();
            try {
                release.await();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        }), "parked");
        t.start();
        parked.await();
        Thread.sleep(200);
        StackTraceElement[] one = t.getStackTrace();
        StackTraceElement[] all = Thread.getAllStackTraces().get(t);
        System.out.println("getStackTrace sees Carrier.run: " + carrierRun(one));
        System.out.println("getStackTrace shows runWith: " + shows(one));
        System.out.println("getAllStackTraces sees Carrier.run: " + carrierRun(all));
        System.out.println("getAllStackTraces shows runWith: " + shows(all));
        release.countDown();
        t.join();
    }
}
