import java.util.concurrent.CountedCompleter;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.ForkJoinTask;
import java.util.concurrent.RecursiveAction;
import java.util.concurrent.RecursiveTask;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.Collections;
import java.util.LinkedHashSet;
import java.util.Set;

/**
 * L5 residual -- WHY a `ForkJoinPool` submission runs its task body twice.
 *
 * The retired lane page (§3) records the symptom and its boundary:
 *
 * <pre>
 *   pool.invoke(task)                computes=2   (HotSpot 1)
 *   pool.execute(task); task.join()  computes=2   (HotSpot 1)
 *   pool.submit(task).get()          computes=2   (HotSpot 1)
 *   task.invoke()                    computes=1   (HotSpot 1)   &lt;- the control
 * </pre>
 *
 * and reads it as "the external submission claimed by both the submitter's
 * help path and a worker". That is a HYPOTHESIS, and it is one of at least
 * three that fit `computes=2`:
 *
 *   (a) two THREADS run the body concurrently -- a real double-claim;
 *   (b) one thread runs it twice, NESTED -- the second entry happening inside
 *       the first, which is what you get when a guard reads a "done" flag that
 *       is only set after the body returns;
 *   (c) one thread runs it twice, SEQUENTIALLY -- two routes into the same
 *       task, the first having finished without recording completion.
 *
 * Those three have different fixes and the symptom cannot tell them apart, so
 * this probe records, per body entry: the THREAD name, the nesting DEPTH at
 * entry, and the order. Then:
 *
 *   threads=2                 -> (a)
 *   threads=1, maxDepth=2     -> (b)
 *   threads=1, maxDepth=1     -> (c)
 *
 * It also reports which ENTRY POINT ran, because `ForkJoinTask`'s three
 * shapes reach the body by different routes (`compute()Ljava/lang/Object;`,
 * `compute()V`, `exec()Z`) and CratonVM picks between them at runtime. A
 * double that shows up on one shape and not the others names the route.
 *
 * Every row uses a FRESH task: a `ForkJoinTask` may be executed once, so
 * re-using one across shapes would measure the re-use rule instead.
 *
 * Deterministic on both VMs -- no timing, no sleeps, no thread counts in the
 * output beyond how many DISTINCT threads touched one body. Check the `rows`
 * trailer before believing a clean diff.
 */
public class L5FjDouble {
    static int rows;

    static void say(String s) {
        rows++;
        System.out.println(s);
    }

    /** Per-body-run evidence, shared by all three task shapes. */
    static final class Rec {
        final AtomicInteger runs = new AtomicInteger();
        final AtomicInteger depth = new AtomicInteger();
        final AtomicInteger maxDepth = new AtomicInteger();
        final Set<String> threads = Collections.synchronizedSet(new LinkedHashSet<>());

        void enter() {
            runs.incrementAndGet();
            threads.add(Thread.currentThread().getName());
            int d = depth.incrementAndGet();
            maxDepth.accumulateAndGet(d, Math::max);
        }

        void leave() {
            depth.decrementAndGet();
        }

        /** `threads` is a COUNT, not a name: carrier names differ between VMs. */
        String verdict() {
            int r = runs.get();
            int t = threads.size();
            int m = maxDepth.get();
            String shape = r <= 1 ? "single"
                    : t > 1 ? "CONCURRENT-two-threads"
                    : m > 1 ? "NESTED-reentrant"
                    : "SEQUENTIAL-same-thread";
            return "runs=" + r + " threads=" + t + " maxDepth=" + m + " " + shape;
        }
    }

    static final class RTask extends RecursiveTask<Integer> {
        final Rec rec;

        RTask(Rec rec) {
            this.rec = rec;
        }

        @Override
        protected Integer compute() {
            rec.enter();
            int v = 0;
            for (int i = 0; i < 1000; i++) {
                v += i;
            }
            rec.leave();
            return v;
        }
    }

    static final class RAction extends RecursiveAction {
        final Rec rec;

        RAction(Rec rec) {
            this.rec = rec;
        }

        @Override
        protected void compute() {
            rec.enter();
            rec.leave();
        }
    }

    static final class CCompleter extends CountedCompleter<Integer> {
        final Rec rec;
        int result;

        CCompleter(Rec rec) {
            super(null);
            this.rec = rec;
        }

        @Override
        public void compute() {
            rec.enter();
            result = 42;
            rec.leave();
            tryComplete();
        }

        @Override
        public Integer getRawResult() {
            return result;
        }
    }

    interface Maker {
        ForkJoinTask<?> make(Rec rec);
    }

    static void shape(String kind, Maker maker, String how) {
        Rec rec = new Rec();
        ForkJoinTask<?> t = maker.make(rec);
        ForkJoinPool pool = new ForkJoinPool(2);
        String err = "";
        try {
            switch (how) {
                case "task.invoke":
                    t.invoke();
                    break;
                case "pool.invoke":
                    pool.invoke(t);
                    break;
                case "pool.execute+join":
                    pool.execute(t);
                    t.join();
                    break;
                case "pool.submit+get":
                    pool.submit(t).get();
                    break;
                case "pool.execute+get":
                    pool.execute(t);
                    t.get();
                    break;
                default:
                    err = " UNKNOWN-SHAPE";
            }
        } catch (Throwable e) {
            err = " threw=" + e.getClass().getName();
        } finally {
            pool.shutdown();
        }
        say(kind + " " + how + " -> " + rec.verdict() + err);
    }

    public static void main(String[] args) {
        // The control first. `task.invoke()` never enters a pool, and the lane
        // page measured it at 1 on both VMs -- so a row that disagrees HERE
        // means something moved underneath this probe rather than in the pool.
        for (String how : new String[] {
            "task.invoke", "pool.invoke", "pool.execute+join", "pool.submit+get", "pool.execute+get"
        }) {
            shape("RecursiveTask ", RTask::new, how);
            shape("RecursiveAction", RAction::new, how);
            shape("CountedCompleter", CCompleter::new, how);
        }

        // The commonPool is a different object with a different registration
        // history, so it gets its own rows rather than an assumption.
        for (String how : new String[] { "pool.invoke", "pool.submit+get" }) {
            Rec rec = new Rec();
            ForkJoinTask<?> t = new RTask(rec);
            String err = "";
            try {
                if (how.equals("pool.invoke")) {
                    ForkJoinPool.commonPool().invoke(t);
                } else {
                    ForkJoinPool.commonPool().submit(t).get();
                }
            } catch (Throwable e) {
                err = " threw=" + e.getClass().getName();
            }
            say("commonPool RecursiveTask " + how + " -> " + rec.verdict() + err);
        }

        // `execute(Runnable)` is the descriptor deliberately left OFF the
        // native keep-list, so it runs the concrete JDK bytecode and its body
        // lands on a real worker. There is no task handle to join, so nothing
        // can double it -- these rows are what says so rather than assuming it.
        // `runs` must be 1; `threads` may legitimately be 1 or 2 and is NOT
        // printed for that reason: which thread ran it is the thing that
        // differs between the two VMs by design here.
        for (int i = 0; i < 2; i++) {
            Rec rr = new Rec();
            ForkJoinPool pool = new ForkJoinPool(2);
            String err = "";
            try {
                pool.execute(() -> {
                    rr.enter();
                    rr.leave();
                });
                pool.shutdown();
                pool.awaitTermination(10, java.util.concurrent.TimeUnit.SECONDS);
            } catch (Throwable e) {
                err = " threw=" + e.getClass().getName();
            }
            say("execute(Runnable) run=" + i + " -> runs=" + rr.runs.get()
                    + " maxDepth=" + rr.maxDepth.get() + err);
        }

        // A task run twice through the SAME route is the re-use rule, not the
        // defect -- recorded so the rows above cannot be read as one.
        Rec rec = new Rec();
        RTask t = new RTask(rec);
        String second = "";
        try {
            t.invoke();
            t.invoke();
        } catch (Throwable e) {
            second = " secondThrew=" + e.getClass().getName();
        }
        say("reuse task.invoke twice -> " + rec.verdict() + second);

        System.out.println("rows " + rows);
        System.out.println("DONE L5FjDouble");
    }
}
