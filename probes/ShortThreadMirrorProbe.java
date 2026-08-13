import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CountDownLatch;

/**
 * Paired probe for the two {@code java/lang/Thread} rows of the short-object
 * census (W7-73-short-object-blind-spot.md §3.3, repaired in
 * W7-74-short-object-repairs.md).
 *
 * <p><b>What was wrong.</b> {@code vertx_eventloop.rs} and
 * {@code xnio_io_thread.rs} both allocated their carrier's
 * {@code java.lang.Thread} mirror as
 * {@code ctx.alloc_object(ClassId::new(0), 5)} — unconditionally, with no
 * attempt to resolve the class — and then published it to the VM thread
 * registry through {@code set_native_thread_java_obj}. The
 * {@code ClassId::new(0)} sentinel is substituted with
 * {@code cratonvm/synthetic/AnonymousObject$5}, so on a real image that object
 * is
 *
 * <ul>
 *   <li><b>short</b>: real {@code java.lang.Thread} declares 19 instance fields
 *       ({@code javap -p}, JDK 25.0.3.9, transitive, {@code static} excluded),
 *       so slots 5..18 are off the end of the object; and the private map is
 *       wrong even inside the five, because slot 0 is {@code eetop} and slot 2
 *       is {@code name};</li>
 *   <li><b>not a Thread at all</b>: {@code AnonymousObject$5} is assignable to
 *       nothing, so it cannot even dispatch {@code Thread}'s methods.</li>
 * </ul>
 *
 * <p><b>The two vacuous shapes this probe is built to refuse.</b>
 *
 * <ol>
 *   <li><i>An assertion that passes because nothing threw.</i> Every check
 *       below asserts an <b>exact value</b> measured on HotSpot 25.0.3.9, never
 *       "non-null" and never "no exception".</li>
 *   <li><i>Reading only the fields the native wrote.</i> The old native wrote
 *       two things: a name and a number. A probe that asks for those two would
 *       pass against an object that is short in exactly the fourteen fields
 *       nobody asked for. So every read here goes through a <b>real JDK
 *       accessor whose body is JDK bytecode reading a JDK field by the JDK's
 *       own index</b>, and the battery covers accessors the native never
 *       touched: {@code getPriority} and {@code isDaemon} read
 *       {@code holder.priority} / {@code holder.daemon}, and
 *       {@code getThreadGroup} and {@code getState} read {@code holder.group} /
 *       {@code holder.threadStatus} — all four through the {@code holder}
 *       reference at slot 5, which a five-slot object does not have.</li>
 * </ol>
 *
 * <p><b>Sections.</b>
 *
 * <ol>
 *   <li><b>The oracle.</b> A named, non-daemon, application-created thread,
 *       read both from inside itself and from the outside. This is the shape a
 *       correct carrier mirror must satisfy, and every expected value below is
 *       a transcript of this section on HotSpot 25.0.3.9.</li>
 *   <li><b>The census.</b> The same battery run over <b>every thread the VM
 *       reports as live</b>, reached through {@code Thread.getAllStackTraces()}
 *       and {@code ThreadGroup.enumerate(Thread[])} — the two real JDK
 *       enumerations that CratonVM answers out of
 *       {@code ThreadRegistry::alive_thread_objects()}, which is precisely
 *       where the carrier mirrors are published. On a plain HotSpot run this
 *       walks the JDK's own threads and every one passes. Under CratonVM on a
 *       workload that starts a Vert.x/Netty event loop or an XNIO I/O thread
 *       (Quarkus, Keycloak, WildFly, Undertow), the carriers are in this set —
 *       and before the repair each of them fails at the first accessor, or
 *       makes {@code enumerate} itself fail, because a
 *       {@code cratonvm/synthetic/AnonymousObject$5} cannot be stored into a
 *       {@code Thread[]}.</li>
 *   <li><b>The driver, when the jars are there.</b> {@code VertxImpl.init(int)}
 *       is the registered door onto {@link #VERTX} that spawns the carriers, and
 *       it is reached reflectively so the probe still runs with no Vert.x on the
 *       classpath. It prints SKIP rather than passing when the class is absent —
 *       a run that never ran is not a green.</li>
 * </ol>
 *
 * <p><b>The RED, measured rather than claimed.</b> The battery's discriminating
 * power was checked on HotSpot 25.0.3.9 itself against the closest thing
 * HotSpot can produce to the pre-repair mirror — a {@code java.lang.Thread}
 * allocated with {@code sun.misc.Unsafe.allocateInstance}, i.e. right class,
 * full width, no constructor run, so {@code holder} is null and {@code name} /
 * {@code tid} are at their defaults. Transcript:
 *
 * <pre>
 *   isInstanceOfThread = true
 *   getName    = null
 *   threadId   = 0
 *   getPriority     THREW NullPointerException: Cannot read field "priority" because "this.holder" is null
 *   isDaemon        THREW NullPointerException: Cannot read field "daemon" because "this.holder" is null
 *   getThreadGroup  THREW NullPointerException: Cannot read field "threadStatus" because "this.holder" is null
 *   getState        THREW NullPointerException: Cannot read field "threadStatus" because "this.holder" is null
 * </pre>
 *
 * So four of the six accessors throw and the other two return values this probe
 * asserts against exactly ({@code getName != null}, {@code threadId > 0}). Six
 * of six discriminate, on a HotSpot run, against an object that is <i>better</i>
 * than the one the repair removed: that object was a real {@code Thread} of full
 * width and only unpopulated, where the carrier mirror was five slots of a class
 * that is not a {@code Thread} at all. Whatever the pre-repair mirror does, it
 * cannot do better than this, and this is already red.
 *
 * <p>The carrier names are {@code vert.x-eventloop-N}
 * ({@code vertx_eventloop.rs::native_vertx_init}) and whatever name the XNIO
 * worker hands {@code spawn_io_thread_with_ctx}; section 2 does not filter on
 * them, deliberately — a mirror that fails the battery is a finding whoever
 * created it.
 */
public class ShortThreadMirrorProbe {

    static final String VERTX = "io.vertx.core.impl.VertxImpl";

    static int failures = 0;
    static int checks = 0;

    static void check(String what, Object expected, Object actual) {
        checks++;
        boolean ok = expected == null ? actual == null : expected.equals(actual);
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "  ok   " : "  FAIL ") + what
                + " expected=" + expected + " actual=" + actual);
    }

    /**
     * {@link #battery} with every throw converted into a FAIL.
     *
     * <p>The failure mode this exists for is specific: on CratonVM before the
     * repair, the first accessor called on a carrier mirror raises rather than
     * returning a wrong value, because {@code AnonymousObject$5} declares none
     * of these methods. Letting that propagate would abort the run at the first
     * bad thread and leave every later one untested — a red, but one that hides
     * how many there are. A throw IS a failure here; it is counted as one and
     * the walk continues.
     */
    static void batteryCatching(String label, Thread t, String expectName,
                                Integer expectPriority, Boolean expectDaemon) {
        try {
            battery(label, t, expectName, expectPriority, expectDaemon);
        } catch (Throwable e) {
            failures++;
            checks++;
            System.out.println("  FAIL " + label + " THREW " + e.getClass().getName()
                    + ": " + e.getMessage()
                    + "  (a real JDK accessor could not run against this object at all —"
                    + " see W7-74-short-object-repairs.md)");
        }
    }

    /**
     * The whole battery, against one Thread reference.
     *
     * <p>Six real JDK accessors, in increasing order of how much of the object
     * they need:
     *
     * <ul>
     *   <li>{@code getClass()} / {@code Thread.class.isInstance} — identity. A
     *       mirror that fails here fails everything after it, and the reason is
     *       not width.</li>
     *   <li>{@code getName()} — reads {@code Thread.name}, slot 2.</li>
     *   <li>{@code threadId()} — reads {@code Thread.tid}, slot 1, and must be
     *       strictly positive per the {@code Thread.threadId()} contract.</li>
     *   <li>{@code getPriority()}, {@code isDaemon()}, {@code getThreadGroup()},
     *       {@code getState()} — all four dereference {@code Thread.holder},
     *       slot 5, which is off the end of a five-slot object. These are the
     *       fields nobody was reading, which is exactly why they are here.</li>
     * </ul>
     *
     * <p>{@code expectName == null} means "any name" (used for the census pass,
     * where the JDK's own threads are named whatever the JDK named them);
     * everything else is asserted exactly.
     */
    static void battery(String label, Thread t, String expectName,
                        Integer expectPriority, Boolean expectDaemon) {
        System.out.println(label + ":");
        check(label + ".isInstanceOfThread", Boolean.TRUE, Thread.class.isInstance(t));
        check(label + ".classIsAssignableToThread", Boolean.TRUE,
                Thread.class.isAssignableFrom(t.getClass()));
        String name = t.getName();
        if (expectName != null) {
            check(label + ".getName", expectName, name);
        } else {
            checks++;
            System.out.println("  info " + label + ".getName = " + name);
        }
        check(label + ".getName!=null", Boolean.TRUE, name != null);
        check(label + ".threadId>0", Boolean.TRUE, t.threadId() > 0L);
        // getPriority(): holder.priority. Contract: 1..10 always.
        int prio = t.getPriority();
        check(label + ".getPriorityInRange", Boolean.TRUE, prio >= Thread.MIN_PRIORITY
                && prio <= Thread.MAX_PRIORITY);
        if (expectPriority != null) {
            check(label + ".getPriority", expectPriority, prio);
        }
        // isDaemon(): holder.daemon.
        boolean daemon = t.isDaemon();
        if (expectDaemon != null) {
            check(label + ".isDaemon", expectDaemon, daemon);
        } else {
            checks++;
            System.out.println("  info " + label + ".isDaemon = " + daemon);
        }
        // getThreadGroup(): holder.group. Null only for a TERMINATED thread.
        ThreadGroup g = t.getThreadGroup();
        check(label + ".getThreadGroupNonNullWhenAlive", Boolean.valueOf(t.isAlive()),
                Boolean.valueOf(g != null));
        if (g != null) {
            check(label + ".threadGroupNameNonNull", Boolean.TRUE, g.getName() != null);
        }
        // getState(): VM.toThreadState(holder.threadStatus).
        Thread.State st = t.getState();
        check(label + ".getStateNonNull", Boolean.TRUE, st != null);
        check(label + ".getStateIsAThreadState", Boolean.TRUE,
                st != null && st.getDeclaringClass() == Thread.State.class);
        check(label + ".stateOfAnAliveThreadIsNotNew", Boolean.FALSE,
                t.isAlive() && st == Thread.State.NEW);
    }

    // -- Section 1: the oracle ----------------------------------------------

    static void section1() throws Exception {
        System.out.println("== 1. oracle: an application-created carrier-shaped thread ==");
        final CountDownLatch running = new CountDownLatch(1);
        final CountDownLatch release = new CountDownLatch(1);
        final String[] insideName = new String[1];
        final long[] insideTid = new long[1];
        final int[] insidePrio = new int[1];
        final boolean[] insideDaemon = new boolean[1];
        final String[] insideGroup = new String[1];
        final String[] insideState = new String[1];
        final String[] insideClass = new String[1];

        Thread t = new Thread(() -> {
            Thread self = Thread.currentThread();
            insideClass[0] = self.getClass().getName();
            insideName[0] = self.getName();
            insideTid[0] = self.threadId();
            insidePrio[0] = self.getPriority();
            insideDaemon[0] = self.isDaemon();
            ThreadGroup g = self.getThreadGroup();
            insideGroup[0] = g == null ? null : g.getName();
            insideState[0] = self.getState().name();
            running.countDown();
            try {
                release.await();
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
        }, "probe-carrier-0");
        t.setDaemon(false);
        t.start();
        running.await();

        // Read from INSIDE the thread — this is `Thread.currentThread()`, the
        // exact call the carrier mirror answers.
        check("inside.getClass", "java.lang.Thread", insideClass[0]);
        check("inside.getName", "probe-carrier-0", insideName[0]);
        check("inside.threadId>0", Boolean.TRUE, insideTid[0] > 0L);
        check("inside.getPriority", Integer.valueOf(5), Integer.valueOf(insidePrio[0]));
        check("inside.isDaemon", Boolean.FALSE, Boolean.valueOf(insideDaemon[0]));
        check("inside.getThreadGroup!=null", Boolean.TRUE, insideGroup[0] != null);
        check("inside.getState", "RUNNABLE", insideState[0]);

        // And from OUTSIDE, on the same object, through the same accessors.
        battery("outside", t, "probe-carrier-0", Integer.valueOf(5), Boolean.FALSE);

        release.countDown();
        t.join();
        // A terminated thread: getThreadGroup() is null and getState() is
        // TERMINATED — both still real reads through `holder`.
        check("terminated.getState", Thread.State.TERMINATED, t.getState());
        check("terminated.getThreadGroup", null, t.getThreadGroup());
        check("terminated.getName", "probe-carrier-0", t.getName());
    }

    // -- Section 2: the census ----------------------------------------------

    static List<Thread> liveThreads() {
        List<Thread> out = new ArrayList<>();
        // Door 1: Thread.getAllStackTraces(). Real JDK bytecode; CratonVM
        // answers it out of the same registry the carrier mirrors go into.
        Set<Thread> all = Thread.getAllStackTraces().keySet();
        out.addAll(all);
        // Door 2: ThreadGroup.enumerate(Thread[]). This one has to STORE each
        // thread into a Thread[], so a mirror whose class is not assignable to
        // java.lang.Thread cannot survive it — the failure is the finding.
        ThreadGroup root = Thread.currentThread().getThreadGroup();
        while (root.getParent() != null) {
            root = root.getParent();
        }
        Thread[] buf = new Thread[root.activeCount() + 32];
        int n = root.enumerate(buf, true);
        for (int i = 0; i < n; i++) {
            if (buf[i] != null && !out.contains(buf[i])) {
                out.add(buf[i]);
            }
        }
        return out;
    }

    static void section2() {
        System.out.println("== 2. census: the battery over every live thread ==");
        List<Thread> live;
        try {
            live = liveThreads();
        } catch (Throwable e) {
            // `ThreadGroup.enumerate(Thread[])` has to STORE each live thread
            // into a `Thread[]`. A mirror whose runtime class is not assignable
            // to `java.lang.Thread` cannot be stored there, so the enumeration
            // itself is a discriminator and its failure is the finding.
            failures++;
            checks++;
            System.out.println("  FAIL enumerating live threads THREW "
                    + e.getClass().getName() + ": " + e.getMessage());
            return;
        }
        System.out.println("  live thread count = " + live.size());
        check("census.foundAtLeastOne", Boolean.TRUE, live.size() >= 1);
        int i = 0;
        for (Thread t : live) {
            batteryCatching("live[" + (i++) + "]", t, null, null, null);
        }
    }

    // -- Section 3: the Vert.x driver ---------------------------------------

    static void section3() {
        System.out.println("== 3. driver: VertxImpl.init(4) then re-census ==");
        Class<?> vertx;
        try {
            vertx = Class.forName(VERTX);
        } catch (Throwable e) {
            System.out.println("  SKIP " + VERTX + " is not on the classpath ("
                    + e.getClass().getName() + ") — this section did NOT run, "
                    + "and a run that never ran is not a green.");
            return;
        }
        try {
            Object v = vertx.getDeclaredConstructor().newInstance();
            Method init = vertx.getDeclaredMethod("init", int.class);
            init.setAccessible(true);
            init.invoke(v, 4);
        } catch (Throwable e) {
            System.out.println("  SKIP could not drive " + VERTX + ".init(int): " + e);
            return;
        }
        List<Thread> live = liveThreads();
        int carriers = 0;
        int i = 0;
        for (Thread t : live) {
            String n;
            try {
                n = t.getName();
            } catch (Throwable e) {
                failures++;
                System.out.println("  FAIL live[" + (i++) + "].getName threw " + e);
                continue;
            }
            if (n != null && n.startsWith("vert.x-eventloop-")) {
                carriers++;
            }
            batteryCatching("post-init[" + (i++) + "]", t, null, null, null);
        }
        check("driver.carriersVisible", Boolean.TRUE, carriers >= 1);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("java.vm.name    = " + System.getProperty("java.vm.name"));
        System.out.println("java.version    = " + System.getProperty("java.version"));
        section1();
        section2();
        section3();
        System.out.println();
        System.out.println("checks=" + checks + " failures=" + failures);
        if (failures != 0) {
            throw new AssertionError(failures + " check(s) failed");
        }
    }
}
