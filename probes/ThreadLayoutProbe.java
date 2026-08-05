import java.lang.reflect.Field;
import java.util.concurrent.*;

/**
 * A layout probe for {@code java.lang.Thread}, diffed against the host JDK.
 *
 * CratonVM's fabricated model declared {@code contextClassLoader} at index 5,
 * where every real image has {@code holder} — the object carrying
 * group/priority/daemon/threadStatus. Both are references, so the overlay
 * hunter's value-tag test could never see it; the L4 shadow-layout diff, which
 * compares by NAME, is what named it.
 *
 * The pairing that matters is {@code contextClassLoader} against everything
 * reached THROUGH {@code holder}: if those two slots are ever confused, the
 * TCCL and the group/priority/daemon triple go wrong together, so this probe
 * reads both after every mutation rather than each on its own.
 *
 * Prints VALUES, never "ok". Run under HotSpot first; its output is expected.
 */
public class ThreadLayoutProbe {
    static int sections = 0, failed = 0;

    static void section(String name, Runnable body) {
        sections++;
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        section("current", ThreadLayoutProbe::current);
        section("tccl", ThreadLayoutProbe::tccl);
        section("holder", ThreadLayoutProbe::holder);
        section("spawned", ThreadLayoutProbe::spawned);
        section("pool", ThreadLayoutProbe::pool);
        section("virtual", ThreadLayoutProbe::virtualThreads);
        section("reflection", ThreadLayoutProbe::reflection);
        System.out.println("THREADLAYOUT sections=" + sections + " failed=" + failed);
    }

    /** Every holder-backed accessor, paired with the TCCL on the same object. */
    static void describe(String tag, Thread t) {
        ClassLoader ccl = t.getContextClassLoader();
        System.out.println(tag
                + " name=" + t.getName()
                + " priority=" + t.getPriority()
                + " daemon=" + t.isDaemon()
                + " alive=" + t.isAlive()
                + " group=" + (t.getThreadGroup() == null ? "null" : t.getThreadGroup().getName())
                + " cclNull=" + (ccl == null)
                + " cclIsLoader=" + (ccl instanceof ClassLoader));
    }

    static void current() {
        describe("current", Thread.currentThread());
    }

    /**
     * The TCCL round-trip. A slot shared with `holder` shows up here as either
     * a lost loader or a corrupted group/priority right after the set.
     */
    static void tccl() {
        Thread t = Thread.currentThread();
        ClassLoader original = t.getContextClassLoader();
        System.out.println("tccl originalIsSystem=" + (original == ClassLoader.getSystemClassLoader()));
        ClassLoader replacement = new java.net.URLClassLoader(new java.net.URL[0], original);
        t.setContextClassLoader(replacement);
        System.out.println("tccl afterSetIdentity=" + (t.getContextClassLoader() == replacement));
        describe("tccl afterSet", t);
        t.setContextClassLoader(null);
        System.out.println("tccl afterNull=" + (t.getContextClassLoader() == null));
        describe("tccl afterNull", t);
        t.setContextClassLoader(original);
        System.out.println("tccl restored=" + (t.getContextClassLoader() == original));
        describe("tccl restored", t);
    }

    /** Mutating a holder-backed field must not disturb the TCCL, and vice versa. */
    static void holder() {
        Thread t = new Thread(() -> { }, "holder-probe");
        ClassLoader marker = new java.net.URLClassLoader(new java.net.URL[0], null);
        t.setContextClassLoader(marker);
        describe("holder beforeMutation", t);
        t.setPriority(3);
        t.setDaemon(true);
        System.out.println("holder cclSurvivedPriorityAndDaemon=" + (t.getContextClassLoader() == marker));
        describe("holder afterMutation", t);
    }

    /** A spawned thread inherits the TCCL, and keeps its own holder state. */
    static void spawned() {
        Thread parent = Thread.currentThread();
        ClassLoader marker = new java.net.URLClassLoader(new java.net.URL[0],
                parent.getContextClassLoader());
        ClassLoader saved = parent.getContextClassLoader();
        parent.setContextClassLoader(marker);
        final String[] seen = new String[3];
        try {
            Thread t = new Thread(() -> {
                Thread me = Thread.currentThread();
                seen[0] = String.valueOf(me.getContextClassLoader() == marker);
                seen[1] = String.valueOf(me.getPriority());
                seen[2] = me.getThreadGroup() == null ? "null" : me.getThreadGroup().getName();
            }, "spawned-probe");
            t.setDaemon(true);
            t.start();
            t.join(5000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        } finally {
            parent.setContextClassLoader(saved);
        }
        System.out.println("spawned inheritedTccl=" + seen[0]
                + " priority=" + seen[1] + " group=" + seen[2]);
    }

    /** Pool threads are where a confused holder shows up as a hang, not a value. */
    static void pool() {
        ExecutorService ex = Executors.newFixedThreadPool(2);
        try {
            String r = ex.submit(() -> {
                Thread me = Thread.currentThread();
                return me.getName().isEmpty() + "/" + me.getPriority() + "/"
                        + (me.getContextClassLoader() != null);
            }).get(10, TimeUnit.SECONDS);
            System.out.println("pool emptyName/priority/hasCcl=" + r);
        } catch (Throwable t) {
            System.out.println("pool THREW " + t.getClass().getName());
        } finally {
            ex.shutdownNow();
        }
    }

    /**
     * The fabricated virtual-thread flag used to share slot 4 with
     * `contextClassLoader`. On a real image a virtual thread is detected by
     * class hierarchy instead, so both must agree here.
     */
    static void virtualThreads() {
        try {
            Thread t = Thread.ofVirtual().name("virt-probe").unstarted(() -> { });
            System.out.println("virtual isVirtual=" + t.isVirtual()
                    + " daemon=" + t.isDaemon());
            final boolean[] inside = new boolean[2];
            Thread run = Thread.ofVirtual().name("virt-run").start(() -> {
                inside[0] = Thread.currentThread().isVirtual();
                inside[1] = Thread.currentThread().getContextClassLoader() != null;
            });
            run.join(5000);
            System.out.println("virtual insideIsVirtual=" + inside[0]
                    + " insideHasCcl=" + inside[1]);
            Thread p = new Thread(() -> { }, "plat-probe");
            System.out.println("virtual platformIsVirtual=" + p.isVirtual());
        } catch (Throwable t) {
            System.out.println("virtual THREW " + t.getClass().getName());
        }
    }

    /** The natives and reflection must agree about the same object. */
    static void reflection() {
        Thread t = Thread.currentThread();
        for (String fn : new String[] {"eetop", "tid", "name", "interrupted",
                                       "contextClassLoader", "holder"}) {
            try {
                Field f = Thread.class.getDeclaredField(fn);
                f.setAccessible(true);
                Object v = f.get(t);
                String shown;
                if (v == null) {
                    shown = "null";
                } else if (v instanceof ClassLoader) {
                    shown = "CL";
                } else if (v instanceof String) {
                    shown = "String";
                } else if (v instanceof Number || v instanceof Boolean) {
                    shown = v.getClass().getSimpleName();
                } else {
                    shown = v.getClass().getName();
                }
                System.out.println("ref " + fn + "=" + shown + " type=" + f.getType().getName());
            } catch (NoSuchFieldException e) {
                System.out.println("ref " + fn + "=NO_SUCH_FIELD");
            } catch (Throwable x) {
                System.out.println("ref " + fn + "=THREW " + x.getClass().getName());
            }
        }
        System.out.println("ref accessorsAgree="
                + (t.getName() != null && t.getPriority() > 0 && t.getThreadGroup() != null));
    }
}
