/**
 * Is `Thread.currentThread()` a STABLE object identity?
 *
 * Hibernate Reactive's `AsyncTrampoline.unroll` bounds recursion by capturing
 * the running thread and comparing it against `Thread.currentThread()` at
 * completion time: same thread => hand the value back through a `PassBack` and
 * let the loop continue; different thread => continue on that thread. The
 * captured `java.lang.Thread` is visible in the lambda descriptor of
 * `lambda$unroll$0(Ljava/lang/Thread;...PassBack;Ljava/lang/Object;Ljava/lang/Throwable;)V`.
 *
 * If `Thread.currentThread()` hands back a DIFFERENT object each call, that
 * reference comparison is false forever, the trampoline recurses instead of
 * unrolling, and stack depth grows without bound — which is what a watchdog
 * dump of `MultithreadedInsertionWithLazyConnectionTest` shows: 22 513 nested
 * interpreter activations on a single event-loop thread.
 */
public class ThreadIdentityProbe {

    static boolean sameTwice(String where) {
        Thread a = Thread.currentThread();
        Thread b = Thread.currentThread();
        boolean refEq = (a == b);
        System.out.println(where
                + ": ref== " + refEq
                + " equals " + a.equals(b)
                + " idHash " + System.identityHashCode(a) + "/" + System.identityHashCode(b)
                + " name " + a.getName() + "/" + b.getName()
                + " tid " + a.threadId() + "/" + b.threadId());
        return refEq;
    }

    /** The shape the trampoline actually uses: capture, then re-read later. */
    static boolean capturedStillMatches(String where) {
        Thread captured = Thread.currentThread();
        Object churn = null;
        for (int i = 0; i < 100000; i++) {
            churn = new Object();          // provoke GC/relocation between reads
        }
        if (churn == null) {
            throw new IllegalStateException();
        }
        Thread now = Thread.currentThread();
        boolean refEq = (captured == now);
        System.out.println(where + " (captured vs later): ref== " + refEq
                + " idHash " + System.identityHashCode(captured) + "/"
                + System.identityHashCode(now));
        return refEq;
    }

    public static void main(String[] args) throws Exception {
        boolean ok = true;
        ok &= sameTwice("main, back to back");
        ok &= capturedStillMatches("main");

        Runnable r = () -> {
            sameTwice("  " + Thread.currentThread().getName() + ", back to back");
            capturedStillMatches("  " + Thread.currentThread().getName());
        };

        Thread t = new Thread(r, "probe-worker");
        t.start();
        t.join();

        // A pool thread, which is what an event loop actually is.
        java.util.concurrent.ExecutorService ex =
                java.util.concurrent.Executors.newSingleThreadExecutor();
        ex.submit(r).get();
        ex.shutdown();

        System.out.println("PROBE-DONE mainStable=" + ok);
    }
}
