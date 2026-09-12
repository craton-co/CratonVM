import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionHandler;
import java.util.concurrent.ScheduledThreadPoolExecutor;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;

/**
 * L5 §11.5 -- the shape `keep_real_scheduled_executor_bridge` is keeping.
 *
 * Two `ScheduledThreadPoolExecutor` triples are kept as `Bridge` natives in
 * real-JDK mode with a comment naming Spring's `ThreadPoolTaskScheduler`:
 *
 * <pre>
 *   &lt;init&gt;(ILjava/util/concurrent/ThreadFactory;Ljava/util/concurrent/RejectedExecutionHandler;)V
 *   getCorePoolSize()I
 * </pre>
 *
 * Both passed all four retirement preconditions and both came out of
 * `RETIRED_SHADOW_L5_TRIPLES` anyway, because a retirement table is MODE-BLIND:
 * the `Bridge -&gt; SyntheticStub` re-tag in `NativeMethodRegistry::register`
 * runs BEFORE the keep predicate reads the kind, so a table entry takes the
 * native away in real-JDK mode too (§4a). Retiring them therefore means
 * DELETING the keep arm and adding the table rows in one change, and that needs
 * evidence from the corpus the keep arm was written for.
 *
 * This probe is the cheap half of that evidence: the SHAPE, in isolation, with
 * no Spring on the classpath. Spring's scheduler is an anonymous subclass whose
 * constructor delegates to `ThreadPoolExecutor`'s real constructor and whose
 * `getCorePoolSize()` resolves the inherited field by name -- so the rows that
 * matter are "does the 3-arg constructor build a working pool" and "does the
 * inherited getter answer what the constructor was handed".
 *
 * It is NOT a substitute for the corpus run. A shape that works here and fails
 * under Spring is exactly what the keep arm's comment is claiming, and only the
 * corpus can refute that.
 */
public class L5StpeKeepArm {
    static int rows;

    static void say(String s) {
        rows++;
        System.out.println(s);
    }

    interface Call {
        Object run() throws Throwable;
    }

    static void probe(String name, Call c) {
        String out;
        try {
            out = "ok=" + c.run();
        } catch (Throwable t) {
            Throwable r = t;
            while (r instanceof java.lang.reflect.InvocationTargetException && r.getCause() != null) {
                r = r.getCause();
            }
            out = "EX:" + r.getClass().getSimpleName();
        }
        say(name + " -> " + out);
    }

    /** The Spring shape: an anonymous subclass built through the 3-arg ctor. */
    static ScheduledThreadPoolExecutor springShape(int core) {
        ThreadFactory tf = Executors.defaultThreadFactory();
        RejectedExecutionHandler h = new ThreadPoolExecutor.AbortPolicy();
        return new ScheduledThreadPoolExecutor(core, tf, h) {
            // Empty on purpose: Spring's own subclass overrides hooks this
            // probe does not need, and an override here would measure the
            // override rather than the constructor.
        };
    }

    public static void main(String[] args) throws Exception {
        probe("3-arg ctor builds", () -> springShape(3) != null);

        probe("getCorePoolSize answers what the ctor was handed", () -> {
            ScheduledThreadPoolExecutor e = springShape(3);
            int seen = e.getCorePoolSize();
            e.shutdown();
            return seen;
        });

        probe("setCorePoolSize is visible to the getter", () -> {
            ScheduledThreadPoolExecutor e = springShape(1);
            e.setCorePoolSize(4);
            int seen = e.getCorePoolSize();
            e.shutdown();
            return seen;
        });

        probe("the pool actually runs a task", () -> {
            ScheduledThreadPoolExecutor e = springShape(2);
            java.util.concurrent.atomic.AtomicInteger n =
                    new java.util.concurrent.atomic.AtomicInteger();
            e.submit(n::incrementAndGet).get(30, TimeUnit.SECONDS);
            e.shutdown();
            return n.get();
        });

        probe("schedule() fires", () -> {
            ScheduledThreadPoolExecutor e = springShape(2);
            Object v = e.schedule(() -> 7, 1, TimeUnit.MILLISECONDS).get(30, TimeUnit.SECONDS);
            e.shutdown();
            return v;
        });

        probe("the inherited ThreadPoolExecutor getters agree", () -> {
            ScheduledThreadPoolExecutor e = springShape(5);
            String s = e.getCorePoolSize() + "/" + e.getMaximumPoolSize()
                    + "/" + (e.getThreadFactory() != null)
                    + "/" + (e.getRejectedExecutionHandler() != null);
            e.shutdown();
            return s;
        });

        probe("shutdown/awaitTermination", () -> {
            ScheduledThreadPoolExecutor e = springShape(1);
            e.shutdown();
            return e.awaitTermination(30, TimeUnit.SECONDS) && e.isShutdown();
        });

        System.out.println("rows " + rows);
        System.out.println("DONE L5StpeKeepArm");
    }
}
