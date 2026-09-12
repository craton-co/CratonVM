import java.lang.reflect.Field;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.ForkJoinTask;
import java.util.concurrent.RecursiveAction;
import java.util.concurrent.RecursiveTask;

/**
 * L5 §11.4 -- are there TWO sources of truth for "is this task done", and can
 * the second one be read at all?
 *
 * §3a diagnosed the ForkJoin double as a race between two completion models:
 * `fjp_state` (the VM's side table, where these natives record `done`) and the
 * JDK's own write-once `status` word, with nothing claiming a task before its
 * body runs. §11.4 records the remedy as "move the pool and task surface onto a
 * single model", which is a subsystem change -- and the first question any such
 * change has to answer is whether the JDK's field is REACHABLE from this VM at
 * all. The side table exists because `ctx.get_field(this, 1)` was not `done`
 * under a real layout; that is an argument against reaching the field BY INDEX,
 * and it is silent about reaching it by name.
 *
 * This probe answers that from Java, with no build, by asking the two models
 * the same question after each shape completes:
 *
 * <pre>
 *   shape -&gt; joined=&lt;v&gt; isDone=&lt;b&gt; statusDone=&lt;b&gt; raw=&lt;v&gt;
 * </pre>
 *
 *   * `isDone()` is whatever this VM answers -- the side table, when the
 *     natives are live;
 *   * `statusDone` is bit 31 of the REAL `ForkJoinTask.status` field, read
 *     reflectively, which is the JDK's own answer and the one a real worker
 *     thread running `doExec()` consults;
 *   * `raw` is `getRawResult()`.
 *
 * On HotSpot all three agree by construction. A row where they DISAGREE is the
 * two-models fact stated as a measurement rather than as a diagnosis -- and a
 * row where `status` cannot be read at all says the single-model remedy needs
 * something other than this field.
 *
 * Needs `--add-opens java.base/java.util.concurrent=ALL-UNNAMED`; without it
 * every `statusDone` reads `NO-ACCESS` and the probe says so rather than
 * printing a silent `false`, which would read exactly like the defect.
 */
public class L5FjStatus {
    static int rows;
    static Field STATUS;
    static String statusErr;

    /** `ForkJoinTask.DONE` -- `1 << 31`, the bit `setDone()` ORs in. */
    static final int DONE = 1 << 31;

    static void say(String s) {
        rows++;
        System.out.println(s);
    }

    static String statusDone(ForkJoinTask<?> t) {
        if (STATUS == null) return "NO-ACCESS:" + statusErr;
        try {
            return String.valueOf((STATUS.getInt(t) & DONE) != 0);
        } catch (Throwable e) {
            return "THREW:" + e.getClass().getSimpleName();
        }
    }

    static void row(String shape, Object joined, ForkJoinTask<?> t) {
        String raw;
        try {
            raw = String.valueOf(t.getRawResult());
        } catch (Throwable e) {
            raw = "THREW:" + e.getClass().getSimpleName();
        }
        say(shape + " -> joined=" + joined
                + " isDone=" + t.isDone()
                + " statusDone=" + statusDone(t)
                + " raw=" + raw);
    }

    /** Counts its own entries, so a body run twice is visible in the value. */
    static class CountingTask extends RecursiveTask<Integer> {
        int runs;

        protected Integer compute() {
            runs++;
            return 40 + runs;
        }
    }

    static class CountingAction extends RecursiveAction {
        int runs;

        protected void compute() {
            runs++;
        }
    }

    /** Never runs: the subject of the cancel row. Deliberately NOT called
     *  `CountedCompleter` -- that is a real `java.util.concurrent` class and a
     *  nested one wearing its name reads like the JDK's in every stack trace
     *  this probe prints. */
    static class NeverRunTask extends RecursiveTask<Integer> {
        protected Integer compute() {
            return 1;
        }
    }

    /** Completes abnormally, which is the other half of the status word. */
    static class ThrowingTask extends RecursiveTask<Integer> {
        protected Integer compute() {
            throw new IllegalStateException("probe");
        }
    }

    static String exc(ForkJoinTask<?> t) {
        try {
            Throwable e = t.getException();
            return e == null ? "null" : e.getClass().getSimpleName();
        } catch (Throwable e) {
            return "THREW:" + e.getClass().getSimpleName();
        }
    }

    public static void main(String[] args) throws Exception {
        try {
            STATUS = ForkJoinTask.class.getDeclaredField("status");
            STATUS.setAccessible(true);
        } catch (Throwable e) {
            statusErr = e.getClass().getSimpleName();
        }
        say("status field readable -> " + (STATUS != null ? "yes" : "no:" + statusErr));

        // A fresh task's status must be zero on both models before anything
        // runs. Without this row a VM that never touches `status` and a VM that
        // sets it correctly are indistinguishable in the rows below, because
        // both would read `false` there for different reasons.
        CountingTask fresh = new CountingTask();
        say("before any run -> isDone=" + fresh.isDone()
                + " statusDone=" + statusDone(fresh)
                + " runs=" + fresh.runs);

        CountingTask a = new CountingTask();
        a.fork();
        row("RecursiveTask fork+join", a.join(), a);
        say("  runs=" + a.runs);

        CountingTask b = new CountingTask();
        row("RecursiveTask invoke", b.invoke(), b);
        say("  runs=" + b.runs);

        CountingAction c = new CountingAction();
        c.fork();
        c.join();
        row("RecursiveAction fork+join", "void", c);
        say("  runs=" + c.runs);

        CountingTask d = new CountingTask();
        ForkJoinPool.commonPool().execute(d);
        row("commonPool execute+join", d.join(), d);
        say("  runs=" + d.runs);

        CountingTask e = new CountingTask();
        row("commonPool submit+get", ForkJoinPool.commonPool().submit(e).get(), e);
        say("  runs=" + e.runs);

        // A task that has completed must stay completed through a second
        // `join()`: that is the read a real worker and the caller both make,
        // and the shape in which the double showed up.
        CountingTask f = new CountingTask();
        f.fork();
        f.join();
        row("second join on a done task", f.join(), f);
        say("  runs=" + f.runs);

        // ---- the two abnormal completions ----
        //
        // `getException()` is NOT registered as a native here, so it runs the
        // image's own bytecode and reads the REAL status word:
        //
        //   s >= 0                      -> null
        //   (s & (ABNORMAL|THROWN)) == ABNORMAL -> new CancellationException()
        //   otherwise                   -> getThrowableException()
        //
        // With the status word left at zero it answers `null` for every task,
        // including a cancelled one, where the JDK's contract is a
        // `CancellationException`. So these rows measure the side effect of
        // agreeing with the real model, not just the agreement itself.
        NeverRunTask g = new NeverRunTask();
        say("cancel before run -> ok=" + g.cancel(false)
                + " isCancelled=" + g.isCancelled()
                + " isDone=" + g.isDone()
                + " statusDone=" + statusDone(g)
                + " getException=" + exc(g));

        ThrowingTask h = new ThrowingTask();
        h.fork();
        String joined;
        try {
            h.join();
            joined = "RETURNED-WITHOUT-THROWING";
        } catch (Throwable t) {
            joined = "EX:" + t.getClass().getSimpleName();
        }
        say("task that threw -> joined=" + joined
                + " isCompletedAbnormally=" + h.isCompletedAbnormally()
                + " statusDone=" + statusDone(h)
                + " getException=" + exc(h));

        System.out.println("rows " + rows);
        System.out.println("DONE L5FjStatus");
    }
}
