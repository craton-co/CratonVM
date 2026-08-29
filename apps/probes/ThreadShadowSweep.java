import java.util.*;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

/** L6 — the `java.lang.Thread` triples the `--jdk-only-report` marks
 *  `outcome=native-won`: a bridge native that RAN in front of real JDK bytecode
 *  rather than losing the dispatch to it.
 *
 *  Method, unchanged from the four families that yielded 28 defects: ask the
 *  CONTRACT EDGES, not the happy path. A shim's middle is where it is most
 *  likely to be right; its refusals, its lifecycle-state rules and its
 *  interrupt bookkeeping are where it was written from memory.
 *
 *  DETERMINISM — this family is the one where it is hardest, so it is spelled
 *  out. NOTHING here prints a thread name the VM chose, a thread id, a
 *  priority the platform defaulted, a stack depth, a timing, a pool size, or an
 *  iteration over a live thread set. Every worker is joined before anything it
 *  produced is read, and every worker is a pure function of its input. The only
 *  values printed are: booleans, thrown exception TYPES, enum constants of
 *  `Thread.State` that the probe itself forced, and class names.
 *
 *  `join(0)` means "wait forever" and is asked with a thread that has already
 *  terminated, so it cannot hang the probe; the never-terminating shape is
 *  asked as `join(1)` on a live thread instead, whose only assertion is that it
 *  RETURNED.
 */
public class ThreadShadowSweep {
    static int rows = 0;

    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    /** Prints the thrown TYPE, which is most of the signal in this family. */
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    /** A thread that does nothing, so its termination is a pure function of
     *  having been started and joined. */
    static Thread idle() { return new Thread(() -> { }); }

    static Thread joined() throws Exception {
        Thread x = idle();
        x.start();
        x.join();
        return x;
    }

    // ---- <init> and the naming contract --------------------------------
    static void construction() {
        // The JDK's Thread(String) NPEs on a null name; so does setName(null).
        t("new Thread((String) null)", () -> new Thread((String) null));
        t("new Thread(Runnable, null)", () -> new Thread(() -> { }, (String) null));
        t("setName(null)", () -> idle().setName(null));
        // A null Runnable target is LEGAL — run() simply does nothing.
        t("new Thread((Runnable) null)", () -> new Thread((Runnable) null));
        Thread n = new Thread(() -> { }, "l6-fixed-name");
        p("getName after ctor", n.getName());
        n.setName("l6-renamed");
        p("getName after setName", n.getName());
        // Renaming a STARTED thread is legal in the JDK.
        t("setName after start", () -> { Thread x = joined(); x.setName("l6-post"); });
        p("empty name is legal", new Thread(() -> { }, "").getName().isEmpty());
        // The default name is VM-chosen (`Thread-N`), so only its SHAPE is
        // asked — never the number.
        p("default name is non-null", idle().getName() != null);
        p("new Thread((Runnable) null).run() is a no-op",
          runNoThrow(new Thread((Runnable) null)));
    }
    static String runNoThrow(Thread x) {
        try { x.run(); return "no-throw"; }
        catch (Throwable e) { return "THREW " + e.getClass().getName(); }
    }

    // ---- the lifecycle state machine -----------------------------------
    static void lifecycle() throws Exception {
        Thread n = idle();
        p("getState NEW", n.getState());
        p("isAlive NEW", n.isAlive());
        n.start();
        n.join();
        p("getState TERMINATED", n.getState());
        p("isAlive TERMINATED", n.isAlive());

        // start() twice is IllegalThreadStateException — BEFORE and AFTER the
        // thread has finished. A shim that only guards `isAlive` gets the
        // second of these wrong.
        t("start twice (terminated)", () -> { Thread x = joined(); x.start(); });
        final Thread live = idle();
        live.start();
        t("start twice (started)", () -> live.start());
        live.join();

        p("currentThread getState", Thread.currentThread().getState());
        p("currentThread isAlive", Thread.currentThread().isAlive());
        p("currentThread class", Thread.currentThread().getClass().getName());
        p("threadgroup non-null on current",
          Thread.currentThread().getThreadGroup() != null);
        p("threadId positive", Thread.currentThread().threadId() > 0);
        p("getId positive", Thread.currentThread().getId() > 0);
        // Two calls to threadId() on one thread must agree; the VALUE is the
        // VM's to choose, the STABILITY is not.
        p("threadId stable", Thread.currentThread().threadId()
                             == Thread.currentThread().threadId());
        p("currentThread == currentThread",
          Thread.currentThread() == Thread.currentThread());
    }

    // ---- daemon and priority -------------------------------------------
    static void daemonAndPriority() throws Exception {
        Thread n = idle();
        p("new thread inherits daemon of main", n.isDaemon());
        n.setDaemon(true);
        p("isDaemon after setDaemon(true)", n.isDaemon());
        n.setDaemon(false);
        p("isDaemon after setDaemon(false)", n.isDaemon());
        // setDaemon on a STARTED thread is IllegalThreadStateException, and on a
        // TERMINATED one it is too — the JDK's guard is `isAlive() ||
        // !isVirtual()`-shaped but the terminated case still throws.
        final Thread live = new Thread(() -> {
            try { Thread.sleep(200); } catch (InterruptedException ignored) { }
        });
        live.start();
        t("setDaemon while alive", () -> live.setDaemon(true));
        live.interrupt();
        live.join();
        t("setDaemon after termination", () -> live.setDaemon(true));

        Thread q = idle();
        p("default priority is in range",
          q.getPriority() >= Thread.MIN_PRIORITY && q.getPriority() <= Thread.MAX_PRIORITY);
        q.setPriority(Thread.MIN_PRIORITY);
        p("getPriority after MIN", q.getPriority());
        q.setPriority(Thread.MAX_PRIORITY);
        p("getPriority after MAX", q.getPriority());
        t("setPriority(0)", () -> idle().setPriority(0));
        t("setPriority(11)", () -> idle().setPriority(11));
        t("setPriority(-1)", () -> idle().setPriority(-1));
        t("setPriority(Integer.MIN_VALUE)", () -> idle().setPriority(Integer.MIN_VALUE));
        p("MIN_PRIORITY", Thread.MIN_PRIORITY);
        p("NORM_PRIORITY", Thread.NORM_PRIORITY);
        p("MAX_PRIORITY", Thread.MAX_PRIORITY);
    }

    // ---- interrupt: the three methods and their clearing rules ----------
    static void interrupts() throws Exception {
        Thread me = Thread.currentThread();
        p("interrupted() clean", Thread.interrupted());
        p("isInterrupted() clean", me.isInterrupted());

        me.interrupt();
        // isInterrupted() does NOT clear; interrupted() does. Asking
        // isInterrupted twice must give the same answer, and interrupted()
        // twice must give true then false. This is the single most-confused
        // pair in the class.
        p("isInterrupted after interrupt (1)", me.isInterrupted());
        p("isInterrupted after interrupt (2)", me.isInterrupted());
        p("interrupted (1)", Thread.interrupted());
        p("interrupted (2)", Thread.interrupted());
        p("isInterrupted after interrupted() cleared", me.isInterrupted());

        // sleep() on an interrupted thread throws AND clears the flag.
        me.interrupt();
        t("sleep while interrupted", () -> Thread.sleep(50));
        p("isInterrupted after InterruptedException", me.isInterrupted());

        // join() on an interrupted thread throws and clears too.
        me.interrupt();
        final Thread live = new Thread(() -> {
            try { Thread.sleep(300); } catch (InterruptedException ignored) { }
        });
        live.start();
        t("join while interrupted", () -> live.join());
        p("isInterrupted after join threw", me.isInterrupted());
        live.interrupt();
        live.join();

        // Interrupting a NEW thread is legal and sets no state a later start()
        // can observe... except that the JDK does record it. Only the calls
        // themselves are asked, never a race.
        Thread n = idle();
        t("interrupt a NEW thread", () -> n.interrupt());
        p("isInterrupted on NEW after interrupt", n.isInterrupted());
        // Interrupting a TERMINATED thread is a no-op that must not throw.
        Thread d = joined();
        t("interrupt a TERMINATED thread", () -> d.interrupt());
        p("isInterrupted on TERMINATED", d.isInterrupted());

        // A worker observing its own interrupt: joined before read, so this is
        // a pure function of the interrupt having been delivered.
        final AtomicReference<String> seen = new AtomicReference<>("never-ran");
        final Object gate = new Object();
        Thread w = new Thread(() -> {
            try {
                synchronized (gate) { gate.wait(2000); }
                seen.set("returned-without-interrupt");
            } catch (InterruptedException e) {
                seen.set("InterruptedException, flag=" + Thread.currentThread().isInterrupted());
            }
        });
        w.start();
        // Wait until the worker is parked in wait() before interrupting, so the
        // outcome does not depend on scheduling.
        for (int i = 0; i < 400 && w.getState() != Thread.State.WAITING
                        && w.getState() != Thread.State.TIMED_WAITING; i++) {
            Thread.sleep(5);
        }
        w.interrupt();
        w.join();
        p("worker saw", seen.get());
    }

    // ---- join: the argument contract ------------------------------------
    static void joins() throws Exception {
        Thread d = joined();
        // join(0) is "forever"; on an already-terminated thread it returns at
        // once, which is how it can be asked at all.
        t("join(0) on terminated", () -> d.join(0));
        t("join() on terminated", () -> d.join());
        t("join(5) on terminated", () -> d.join(5));
        t("join(0, 0) on terminated", () -> d.join(0, 0));
        // Negative millis is IllegalArgumentException; negative/oversized nanos
        // likewise. These are the guards a shim written from memory omits.
        t("join(-1)", () -> d.join(-1));
        t("join(-1, 0)", () -> d.join(-1, 0));
        t("join(0, -1)", () -> d.join(0, -1));
        t("join(0, 1000000)", () -> d.join(0, 1000000));
        t("join(1, 999999)", () -> d.join(1, 999999));
        // join() on a LIVE thread with a timeout must RETURN. The assertion is
        // only that it came back, never how long it took.
        final Thread live = new Thread(() -> {
            try { Thread.sleep(400); } catch (InterruptedException ignored) { }
        });
        live.start();
        t("join(1) on a live thread returns", () -> live.join(1));
        p("live thread still alive after join(1)", live.isAlive());
        live.interrupt();
        live.join();
        p("joined thread is not alive", live.isAlive());
        // Joining yourself with a timeout returns; joining yourself forever
        // would deadlock and is deliberately not asked.
        t("self join(1)", () -> Thread.currentThread().join(1));
    }

    // ---- sleep, yield, onSpinWait, holdsLock ----------------------------
    static void statics() throws Exception {
        t("sleep(0)", () -> Thread.sleep(0));
        t("sleep(1)", () -> Thread.sleep(1));
        t("sleep(-1)", () -> Thread.sleep(-1));
        t("sleep(Long.MIN_VALUE)", () -> Thread.sleep(Long.MIN_VALUE));
        t("sleep(0, 0)", () -> Thread.sleep(0, 0));
        t("sleep(0, -1)", () -> Thread.sleep(0, -1));
        t("sleep(0, 1000000)", () -> Thread.sleep(0, 1000000));
        t("sleep(-1, 0)", () -> Thread.sleep(-1, 0));
        t("sleep(null Duration)", () -> Thread.sleep(null));
        t("yield", () -> Thread.yield());
        t("onSpinWait", () -> Thread.onSpinWait());
        t("holdsLock(null)", () -> Thread.holdsLock(null));
        Object lock = new Object();
        p("holdsLock outside", Thread.holdsLock(lock));
        synchronized (lock) { p("holdsLock inside", Thread.holdsLock(lock)); }
        p("holdsLock after", Thread.holdsLock(lock));
        p("activeCount positive", Thread.activeCount() > 0);
        // enumerate() into an UNDERSIZED array must not overflow it; the count
        // is the VM's, the containment is not.
        Thread[] tiny = new Thread[1];
        p("enumerate into size-1 array <= 1", Thread.enumerate(tiny) <= 1);
        t("enumerate(null)", () -> Thread.enumerate(null));
        p("currentThread getContextClassLoader non-null",
          Thread.currentThread().getContextClassLoader() != null);
    }

    // ---- the removed and deprecated methods ------------------------------
    static void removed() throws Exception {
        // In JDK 20+ these throw UnsupportedOperationException rather than
        // doing anything. A shim that implements them is WORSE than one that
        // does not: it reintroduces a hazard the platform removed.
        // `suspend`/`resume` were REMOVED outright in JDK 25 and no longer
        // compile, so they cannot be asked from source at all.
        t("stop()", () -> joined().stop());
        t("checkAccess()", () -> joined().checkAccess());
        p("Thread.currentThread().isVirtual()", Thread.currentThread().isVirtual());
    }

    // ---- uncaught exception handling -------------------------------------
    static void uncaught() throws Exception {
        p("default handler is null unless set",
          Thread.getDefaultUncaughtExceptionHandler() == null);
        Thread n = idle();
        p("per-thread handler defaults to the group",
          n.getUncaughtExceptionHandler() != null);
        final AtomicReference<String> got = new AtomicReference<>("not-called");
        Thread w = new Thread(() -> { throw new IllegalStateException("l6"); });
        w.setUncaughtExceptionHandler((th, e) ->
            got.set(e.getClass().getName() + ":" + e.getMessage()));
        w.start();
        w.join();
        p("uncaught handler saw", got.get());
        p("thread that threw is TERMINATED", w.getState());
        t("setUncaughtExceptionHandler(null)",
          () -> idle().setUncaughtExceptionHandler(null));

        // The DEFAULT handler, installed and removed, so the probe leaves no
        // global state behind.
        final AtomicInteger hits = new AtomicInteger();
        Thread.setDefaultUncaughtExceptionHandler((th, e) -> hits.incrementAndGet());
        Thread v = new Thread(() -> { throw new RuntimeException("l6d"); });
        v.start();
        v.join();
        p("default handler hit count", hits.get());
        Thread.setDefaultUncaughtExceptionHandler(null);
        p("default handler cleared",
          Thread.getDefaultUncaughtExceptionHandler() == null);
    }

    // ---- Thread.Builder (JDK 21+), platform and virtual -------------------
    static void builders() throws Exception {
        t("ofPlatform().unstarted(null)",
          () -> Thread.ofPlatform().unstarted(null));
        Thread pl = Thread.ofPlatform().name("l6-plat").unstarted(() -> { });
        p("ofPlatform name", pl.getName());
        p("ofPlatform isVirtual", pl.isVirtual());
        p("ofPlatform isDaemon default", pl.isDaemon());
        p("ofPlatform state", pl.getState());
        pl.start(); pl.join();
        p("ofPlatform terminated", pl.getState());

        final AtomicReference<String> vseen = new AtomicReference<>("never-ran");
        Thread vt = Thread.ofVirtual().name("l6-virt").unstarted(
            () -> vseen.set("ran, virtual=" + Thread.currentThread().isVirtual()));
        p("ofVirtual isVirtual", vt.isVirtual());
        p("ofVirtual isDaemon", vt.isDaemon());
        p("ofVirtual priority", vt.getPriority());
        vt.start(); vt.join();
        p("ofVirtual worker saw", vseen.get());
        p("ofVirtual terminated", vt.getState());
        t("virtual setDaemon(false)", () -> Thread.ofVirtual().unstarted(() -> { }).setDaemon(false));
        t("virtual setPriority", () -> Thread.ofVirtual().unstarted(() -> { }).setPriority(3));
        // A builder's name(prefix, start) counter, asked only for its SHAPE.
        Thread.Builder.OfPlatform b = Thread.ofPlatform().name("l6-", 7);
        p("builder counter name 1", b.unstarted(() -> { }).getName());
        p("builder counter name 2", b.unstarted(() -> { }).getName());
        t("builder name(null)", () -> Thread.ofPlatform().name(null));
        t("builder name(null, 0)", () -> Thread.ofPlatform().name(null, 0));
        t("builder name(prefix, -1)", () -> Thread.ofPlatform().name("x", -1));
    }

    // ---- ThreadLocal through a thread boundary ---------------------------
    static void threadLocals() throws Exception {
        ThreadLocal<String> tl = ThreadLocal.withInitial(() -> "init");
        InheritableThreadLocal<String> itl = new InheritableThreadLocal<>();
        tl.set("parent");
        itl.set("inherited");
        final AtomicReference<String> child = new AtomicReference<>("never-ran");
        Thread w = new Thread(() -> child.set(tl.get() + "/" + itl.get()));
        w.start();
        w.join();
        p("child sees", child.get());
        tl.remove();
        p("after remove", tl.get());
        itl.remove();
    }

    // ---- getStackTrace: shape only, never depth or line numbers ----------
    static void stacks() throws Exception {
        StackTraceElement[] st = Thread.currentThread().getStackTrace();
        p("own stack is non-empty", st.length > 0);
        boolean sawMain = false;
        for (StackTraceElement e : st) {
            if ("ThreadShadowSweep".equals(e.getClassName()) && "stacks".equals(e.getMethodName())) {
                sawMain = true;
            }
        }
        p("own stack names this method", sawMain);
        Thread d = joined();
        p("terminated thread stack is empty", d.getStackTrace().length == 0);
        Map<Thread, StackTraceElement[]> all = Thread.getAllStackTraces();
        p("getAllStackTraces contains current",
          all.containsKey(Thread.currentThread()));
    }

    public static void main(String[] args) throws Exception {
        construction();
        lifecycle();
        daemonAndPriority();
        interrupts();
        joins();
        statics();
        removed();
        uncaught();
        builders();
        threadLocals();
        stacks();
        System.out.println("rows " + rows + " DONE ThreadShadowSweep");
    }
}
