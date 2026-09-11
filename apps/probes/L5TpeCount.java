import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;

/**
 * L5 -- does the POOL see the task, per submission shape?
 *
 * `ThreadPoolExecutor` has six ways in, and CratonVM serves four of them with
 * one implementation and two of them with another. The two odd ones ran the
 * task inline on the CALLING thread and handed back an already-completed
 * future, so the pool never saw it.
 *
 * The reason this needs its own probe is that nothing an ordinary executor
 * test asserts can tell the difference: the task runs, the future completes,
 * the value is right. What differs is the pool's own accounting -- and the
 * thread the work ran on, which is the half that deadlocks a start-gate
 * `CountDownLatch`.
 *
 * # Why these counters are deterministic
 *
 * They are read AFTER `shutdown()` and a successful `awaitTermination()`. At
 * that point every submitted task has finished, no worker is running, and
 * `getTaskCount()` and `getCompletedTaskCount()` are both exactly the number
 * of tasks the pool accepted -- on any correct implementation, whatever the
 * scheduler did. Nothing here prints a pool size, an active count, a thread
 * name or a timing.
 */
public class L5TpeCount {
    static int rows;

    interface Body {
        void go(ThreadPoolExecutor t) throws Exception;
    }

    static ThreadPoolExecutor mk() {
        return new ThreadPoolExecutor(2, 2, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<>());
    }

    static void run(String what, int n, Body body) throws Exception {
        ThreadPoolExecutor t = mk();
        body.go(t);
        t.shutdown();
        boolean done = t.awaitTermination(60, TimeUnit.SECONDS);
        rows++;
        System.out.println(what + " submitted=" + n
                + " completed=" + t.getCompletedTaskCount()
                + " taskCount=" + t.getTaskCount()
                + " terminated=" + done);
    }

    public static void main(String[] args) throws Exception {
        run("execute", 3, t -> {
            for (int i = 0; i < 3; i++) {
                t.execute(() -> { });
            }
        });
        run("submitCallable", 3, t -> {
            List<Future<Integer>> f = new ArrayList<>();
            for (int i = 0; i < 3; i++) {
                f.add(t.submit(() -> 1));
            }
            for (Future<Integer> x : f) {
                x.get();
            }
        });
        run("submitRunnable", 3, t -> {
            List<Future<?>> f = new ArrayList<>();
            for (int i = 0; i < 3; i++) {
                f.add(t.submit((Runnable) () -> { }));
            }
            for (Future<?> x : f) {
                x.get();
            }
        });
        run("submitRunnableValue", 3, t -> {
            List<Future<String>> f = new ArrayList<>();
            for (int i = 0; i < 3; i++) {
                f.add(t.submit(() -> { }, "v"));
            }
            for (Future<String> x : f) {
                x.get();
            }
        });
        run("invokeAll", 3, t -> t.invokeAll(Arrays.asList(() -> 1, () -> 2, () -> 3)));
        run("invokeAny", 1, t -> t.invokeAny(Arrays.asList(() -> 1)));
        System.out.println("rows " + rows);
        System.out.println("DONE L5TpeCount");
    }
}
