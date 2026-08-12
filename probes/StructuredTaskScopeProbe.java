import java.io.DataInputStream;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Function;
import java.util.function.Predicate;
import java.util.function.Supplier;
import java.util.stream.Collectors;
import java.util.stream.Stream;

/**
 * `java.util.concurrent.StructuredTaskScope` as JEP 505 (fifth preview, JDK 25)
 * actually declares it, measured rather than assumed.
 *
 * WHY THIS IS ALL REFLECTION, and it is not squeamishness. StructuredTaskScope
 * is a preview API. Measured on Adoptium 25.0.3.9:
 *
 *   javac P.java                     -> "StructuredTaskScope is a preview API
 *                                        and is disabled by default"
 *   javac --enable-preview P.java    -> compiles; classfile minor becomes 65535
 *   java -cp . P                     -> UnsupportedClassVersionError: "Preview
 *                                        features are not enabled for P
 *                                        (class file version 69.65535)"
 *   java --enable-preview -cp . P    -> runs
 *
 * So a probe that names the type in its own source can only be compiled and run
 * behind `--enable-preview` on BOTH sides, and CratonVM has no such flag
 * (`cratonvm --enable-preview` is an argument-parse error today). A probe that
 * only runs on one arm of a differential is not a differential. Reflection
 * carries no preview bit — `Class.forName("java.util.concurrent.StructuredTaskScope")`
 * succeeds on plain `java` with no flags — so this file compiles at plain
 * `--release 25` and the SAME class file runs on HotSpot, `--real-jdk` and
 * `--jdk-only`. The preview gating is then itself an observable, printed by the
 * `previewGating` section from the class file's own major/minor, rather than
 * being a precondition for taking the measurement.
 *
 * THREE DISCIPLINES, the same three `probes/ShadowDifferentialProbe.java`
 * states, plus one this API forces:
 *
 *   1. FENCED — a section that throws prints one `SECTION-DIED.x` line instead
 *      of deleting every line after it. A truncated transcript reads exactly
 *      like a short clean run.
 *   2. VALUES, NOT VERDICTS — every line prints its actual content. A line that
 *      prints `ok` cannot diff; `subtask.state.afterJoin=UNAVAILABLE` can.
 *   3. BOUNDED — and here it is load-bearing rather than hygienic. This is a
 *      structured-CONCURRENCY API: `join()` is a blocking wait on threads the
 *      scope started, and a `join()` that waits for a task nothing will ever run
 *      does not fail, it HANGS — the shape behind this tree's recorded
 *      `ForkJoinTask.invokeAll`/`awaitDone` hangs. So every section runs on its
 *      own daemon thread and is joined with a timeout; a section that overruns
 *      prints `SECTION-HUNG.x` and the transcript continues. A daemon thread
 *      also cannot hold the VM open afterwards.
 *
 *   4. OWNER-THREAD SAFE, which falls out of 3 for free. `fork`, `join` and
 *      `close` may only be called by the thread that opened the scope
 *      (`WrongThreadException` otherwise), so each section must run WHOLLY on
 *      one thread. Running the body on the section's own thread satisfies both
 *      requirements with one mechanism.
 */
public class StructuredTaskScopeProbe {

    // --- reflective handles on the JEP 505 surface ---------------------------
    // Resolved once, in `main`, so a section that runs before them is a bug
    // rather than a silent null. If the type is absent entirely the resolution
    // failure is printed and every section then reports its own death, which is
    // the correct transcript for "this VM has no StructuredTaskScope".

    static Class<?> STS;
    static Class<?> JOINER;
    static Class<?> SUBTASK;
    static Class<?> SUBTASK_STATE;
    static Class<?> CONFIGURATION;

    static Method M_OPEN0;
    static Method M_OPEN1;
    static Method M_OPEN2;
    static Method M_FORK_CALLABLE;
    static Method M_FORK_RUNNABLE;
    static Method M_JOIN;
    static Method M_IS_CANCELLED;
    static Method M_CLOSE;
    static Method M_STATE;
    static Method M_GET;
    static Method M_EXCEPTION;

    static void line(String k, Object v) {
        System.out.println(k + "=" + v);
    }

    /**
     * FENCED and BOUNDED and owner-thread-safe in one helper — see the class
     * comment, discipline 3/4. The body gets its own daemon thread so that (a)
     * one thread opens, forks, joins and closes the scope, and (b) a body that
     * blocks forever costs one marker line instead of the rest of the
     * transcript.
     *
     * The bound is generous (8s) on purpose. A tight bound turns a loaded
     * machine into a false positive, and the only run this bound changes is one
     * that is already wrong — nothing here should take more than the ~250ms the
     * timeout sections deliberately sleep for.
     */
    static void section(String name, Runnable body) {
        Thread t = new Thread(() -> {
            try {
                body.run();
            } catch (Throwable x) {
                line("SECTION-DIED." + name, describe(x));
            }
        }, "probe-" + name);
        t.setDaemon(true);
        t.start();
        try {
            t.join(8000);
        } catch (InterruptedException ie) {
            Thread.currentThread().interrupt();
        }
        if (t.isAlive()) {
            line("SECTION-HUNG." + name, "still-running-after-8000ms");
        }
    }

    /**
     * Reflection wraps every application throwable in InvocationTargetException,
     * so the interesting type is always the cause. Printing the wrapper instead
     * would make every failure line read `InvocationTargetException` and diff
     * identically no matter WHAT was thrown — the exact way a probe can look
     * like it is measuring and not be.
     */
    static Throwable unwrap(Throwable t) {
        while (t instanceof InvocationTargetException ite && ite.getCause() != null) {
            t = ite.getCause();
        }
        return t;
    }

    /** Type and message. The message is an observable here: `FailedException` carries the cause. */
    static String describe(Throwable t) {
        t = unwrap(t);
        String m = t.getMessage();
        return t.getClass().getName() + (m == null ? "" : ":" + m);
    }

    /** The thrown type (unwrapped) and message, or `no-throw`. */
    static String thrown(ThrowingRunnable r) {
        try {
            r.run();
            return "no-throw";
        } catch (Throwable t) {
            return describe(t);
        }
    }

    interface ThrowingRunnable {
        void run() throws Throwable;
    }

    /**
     * `Callable` would not do: it declares `throws Exception`, and every call
     * here goes through reflection helpers that declare `throws Throwable`. A
     * supplier that cannot express the checked type of the thing it wraps
     * forces try/catch back into every call site, which is where the verdicts
     * creep in.
     */
    interface ThrowingSupplier {
        Object get() throws Throwable;
    }

    /** A value, or the throwable's TYPE — for the few lines whose message is not diff-stable. */
    static String typeOnly(ThrowingSupplier c) {
        try {
            return String.valueOf(c.get());
        } catch (Throwable t) {
            return unwrap(t).getClass().getName();
        }
    }

    /** A value, or the throwable that stopped us getting it. Never a verdict. */
    static String valueOrThrow(ThrowingSupplier c) {
        try {
            return String.valueOf(c.get());
        } catch (Throwable t) {
            return describe(t);
        }
    }

    // --- reflective shorthands ----------------------------------------------

    static Object open() throws Throwable {
        return M_OPEN0.invoke(null);
    }

    static Object open(Object joiner) throws Throwable {
        return M_OPEN1.invoke(null, joiner);
    }

    static Object open(Object joiner, Function<Object, Object> cfg) throws Throwable {
        return M_OPEN2.invoke(null, joiner, cfg);
    }

    static Object fork(Object scope, Callable<?> task) throws Throwable {
        return M_FORK_CALLABLE.invoke(scope, task);
    }

    static Object join(Object scope) throws Throwable {
        return M_JOIN.invoke(scope);
    }

    static void close(Object scope) throws Throwable {
        M_CLOSE.invoke(scope);
    }

    static Object joiner(String factory) throws Throwable {
        return JOINER.getMethod(factory).invoke(null);
    }

    static String state(Object subtask) {
        return valueOrThrow(() -> M_STATE.invoke(subtask));
    }

    /**
     * `close()` in a finally, but never allowed to replace the exception that is
     * the observable. JEP 505's `close()` itself throws when the owner forked
     * and did not join, so a bare `finally { close(); }` can overwrite the very
     * throwable the section exists to print.
     */
    static void closeQuietly(String tag, Object scope) {
        if (scope == null) {
            return;
        }
        line(tag + ".close", thrown(() -> close(scope)));
    }

    public static void main(String[] args) throws Exception {
        line("runtime.version", Runtime.version());
        line("java.vm.name", System.getProperty("java.vm.name"));

        String resolveFailure = null;
        try {
            STS = Class.forName("java.util.concurrent.StructuredTaskScope");
            JOINER = Class.forName("java.util.concurrent.StructuredTaskScope$Joiner");
            SUBTASK = Class.forName("java.util.concurrent.StructuredTaskScope$Subtask");
            SUBTASK_STATE = Class.forName("java.util.concurrent.StructuredTaskScope$Subtask$State");
            CONFIGURATION = Class.forName("java.util.concurrent.StructuredTaskScope$Configuration");
            M_OPEN0 = STS.getMethod("open");
            M_OPEN1 = STS.getMethod("open", JOINER);
            M_OPEN2 = STS.getMethod("open", JOINER, Function.class);
            M_FORK_CALLABLE = STS.getMethod("fork", Callable.class);
            M_FORK_RUNNABLE = STS.getMethod("fork", Runnable.class);
            M_JOIN = STS.getMethod("join");
            M_IS_CANCELLED = STS.getMethod("isCancelled");
            M_CLOSE = STS.getMethod("close");
            M_STATE = SUBTASK.getMethod("state");
            M_GET = SUBTASK.getMethod("get");
            M_EXCEPTION = SUBTASK.getMethod("exception");
        } catch (Throwable t) {
            resolveFailure = describe(t);
        }
        line("surface.resolve", resolveFailure == null ? "all-present" : resolveFailure);

        section("declaredSurface", StructuredTaskScopeProbe::declaredSurface);
        section("deadJdk21Names", StructuredTaskScopeProbe::deadJdk21Names);
        section("previewGating", StructuredTaskScopeProbe::previewGating);
        section("openDefault", StructuredTaskScopeProbe::openDefault);
        section("joinWaits", StructuredTaskScopeProbe::joinWaits);
        section("joinerAwaitAll", StructuredTaskScopeProbe::joinerAwaitAll);
        section("joinerAwaitAllSuccessful", StructuredTaskScopeProbe::joinerAwaitAllSuccessful);
        section("joinerAllSuccessful", StructuredTaskScopeProbe::joinerAllSuccessful);
        section("joinerAnySuccessful", StructuredTaskScopeProbe::joinerAnySuccessful);
        section("joinerAllUntil", StructuredTaskScopeProbe::joinerAllUntil);
        section("forkRunnable", StructuredTaskScopeProbe::forkRunnable);
        section("configuration", StructuredTaskScopeProbe::configuration);
        section("timeout", StructuredTaskScopeProbe::timeout);
        section("stateMachine", StructuredTaskScopeProbe::stateMachine);
        section("ownerThread", StructuredTaskScopeProbe::ownerThread);
        section("subtaskCarrier", StructuredTaskScopeProbe::subtaskCarrier);

        // Daemon section threads may still be alive; a hung one must not turn
        // "the probe reported a hang" into "the probe never exited".
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }

    // ------------------------------------------------------------------------
    // 1. What the image DECLARES. This is the section that dates the API: JDK 21
    //    had an abstract CLASS with `ShutdownOnSuccess`/`ShutdownOnFailure`
    //    subclasses; JEP 505 has a sealed INTERFACE with static `open` factories
    //    and a `Joiner`. Printing modifiers and the method list makes the
    //    difference one diff rather than an argument.
    // ------------------------------------------------------------------------
    static void declaredSurface() {
        for (Class<?> c : new Class<?>[] { STS, JOINER, SUBTASK, SUBTASK_STATE, CONFIGURATION }) {
            if (c == null) {
                continue;
            }
            String tag = c.getSimpleName().isEmpty() ? c.getName() : simple(c);
            line(tag + ".isInterface", c.isInterface());
            line(tag + ".modifiers", Modifier.toString(c.getModifiers()));
            line(tag + ".isSealed", c.isSealed());
            line(tag + ".methods", methodsOf(c));
        }
        line("STS.superinterfaces", Arrays.stream(STS.getInterfaces())
                .map(Class::getName).sorted().collect(Collectors.joining(",")));
        line("Subtask.superinterfaces", Arrays.stream(SUBTASK.getInterfaces())
                .map(Class::getName).sorted().collect(Collectors.joining(",")));
        line("Subtask.State.constants", Arrays.stream(SUBTASK_STATE.getEnumConstants())
                .map(String::valueOf).collect(Collectors.joining(",")));
        // The Joiner factories ARE the replacement for the deleted subclasses.
        // Print each factory's returned implementation class: that is the piece
        // a VM re-implementing this has to mint, and a VM that returns the same
        // object for two different policies shows up right here.
        for (String f : new String[] { "awaitAll", "awaitAllSuccessfulOrThrow",
                "allSuccessfulOrThrow", "anySuccessfulResultOrThrow" }) {
            line("Joiner." + f, valueOrThrow(() -> joiner(f).getClass().getName()));
        }
        line("Joiner.allUntil", valueOrThrow(() -> {
            Predicate<Object> p = s -> false;
            return JOINER.getMethod("allUntil", Predicate.class).invoke(null, p).getClass().getName();
        }));
        // Two Joiners of the same kind: distinct instances or a shared one? A
        // Joiner is stateful (`allSuccessfulOrThrow` accumulates subtasks), so
        // sharing one across scopes would be a real defect, and this is the
        // cheapest place to see it.
        line("Joiner.awaitAll.sameInstanceTwice",
                valueOrThrow(() -> joiner("awaitAll") == joiner("awaitAll")));
        line("Joiner.allSuccessfulOrThrow.sameInstanceTwice",
                valueOrThrow(() -> joiner("allSuccessfulOrThrow") == joiner("allSuccessfulOrThrow")));
    }

    static String simple(Class<?> c) {
        String n = c.getName();
        int i = n.lastIndexOf("StructuredTaskScope");
        return i < 0 ? n : n.substring(i);
    }

    /** Sorted `name(paramTypes)ret` with the static/abstract/default distinction, which is the whole point on an interface. */
    static String methodsOf(Class<?> c) {
        List<String> out = new ArrayList<>();
        for (Method m : c.getDeclaredMethods()) {
            if (m.isSynthetic()) {
                continue;
            }
            StringBuilder sb = new StringBuilder();
            if (Modifier.isStatic(m.getModifiers())) {
                sb.append("static ");
            } else if (Modifier.isAbstract(m.getModifiers())) {
                sb.append("abstract ");
            } else {
                sb.append("default ");
            }
            sb.append(m.getName()).append('(');
            sb.append(Arrays.stream(m.getParameterTypes()).map(Class::getSimpleName)
                    .collect(Collectors.joining(",")));
            sb.append(')').append(m.getReturnType().getSimpleName());
            out.add(sb.toString());
        }
        return out.stream().sorted().collect(Collectors.joining(" | "));
    }

    // ------------------------------------------------------------------------
    // 2. The two JDK-21 names. `docs/known-issues/jdk-only/W7-14-fjp-common-factory-bound-by-name.md`
    //    found them hard-coded in `native-builtins/src/phases_late/concurrent.rs`
    //    and did not fix them, because unlike the ForkJoin factory they have no
    //    correct spelling to resolve to — JEP 505 DELETED them. This section
    //    prints the image's own answer so "deleted" is measured, not asserted,
    //    and so the day some future JDK brings a name back the probe says so.
    //
    //    `jdk.incubator.concurrent.*` is the generation BEFORE that: the same
    //    file still registers ~30 natives against it. Same question, one release
    //    older.
    // ------------------------------------------------------------------------
    static void deadJdk21Names() {
        for (String n : new String[] {
                "java.util.concurrent.StructuredTaskScope$ShutdownOnSuccess",
                "java.util.concurrent.StructuredTaskScope$ShutdownOnFailure",
                "java.util.concurrent.StructuredTaskScope$Config",
                "java.util.concurrent.StructuredTaskScope$Configuration",
                "java.util.concurrent.StructuredTaskScope$FailedException",
                "java.util.concurrent.StructuredTaskScope$TimeoutException",
                "java.util.concurrent.StructuredTaskScopeImpl",
                "jdk.incubator.concurrent.StructuredTaskScope",
                "jdk.incubator.concurrent.StructuredTaskScope$Subtask",
                "jdk.incubator.concurrent.StructuredTaskScope$ShutdownOnSuccess",
                "jdk.incubator.concurrent.StructuredTaskScope$ShutdownOnFailure",
                "jdk.internal.misc.ThreadFlock",
        }) {
            line("forName." + n, valueOrThrow(() -> Class.forName(n).getName()));
        }
        // The nested types the interface itself declares, straight from the
        // image. A VM that has invented extra nested types shows up here as an
        // extra entry rather than as a mystery elsewhere.
        line("STS.declaredClasses", valueOrThrow(() -> Arrays.stream(STS.getDeclaredClasses())
                .map(Class::getName).sorted().collect(Collectors.joining(","))));
        // JEP 505 replaced subclassing with a Joiner. `open()` therefore must
        // NOT be reachable by construction: there is no accessible constructor
        // on an interface, and the implementation is not exported.
        line("STS.constructors", valueOrThrow(() -> STS.getDeclaredConstructors().length));
    }

    // ------------------------------------------------------------------------
    // 3. Preview gating, read off the class files rather than assumed. Three
    //    separate facts, and they are easy to conflate:
    //
    //    a) The JDK's OWN StructuredTaskScope.class is NOT preview-flagged —
    //       minor 0. Preview-ness of the API is carried by the
    //       `@PreviewFeature` annotation and enforced by javac, not by the
    //       class file. That is why reflection reaches it with no flags.
    //    b) A USER class compiled against it IS preview-flagged — minor 65535 —
    //       and HotSpot refuses to load it without `--enable-preview`.
    //    c) This probe's own class file must therefore be minor 0, or it is not
    //       running everywhere it claims to.
    //
    //    (c) is the self-check: if `probe.classfile.minor` is ever 65535 this
    //    probe has quietly become HotSpot-only and its "CratonVM agrees" lines
    //    are worthless.
    // ------------------------------------------------------------------------
    static void previewGating() {
        line("STS.classfile.version", classfileVersion("/java/util/concurrent/StructuredTaskScope.class"));
        line("STSImpl.classfile.version", classfileVersion("/java/util/concurrent/StructuredTaskScopeImpl.class"));
        line("probe.classfile.version", classfileVersion(
                "/" + StructuredTaskScopeProbe.class.getName().replace('.', '/') + ".class"));
        line("PreviewFeature.present", valueOrThrow(
                () -> Class.forName("jdk.internal.javac.PreviewFeature").getName()));
        // `PreviewFeatures.isEnabled()` is what the JDK's own code asks. A VM
        // that has no notion of preview at all answers differently here from one
        // that answers `false`, and the two are not the same bug.
        // TYPE ONLY, no message: on a real JDK this is refused by the module
        // system and the refusal message embeds the unnamed module's identity
        // hash, which changes every run. A differential line that cannot be
        // equal to itself twice is worse than no line — it trains the reader to
        // ignore diffs. (Measured: three consecutive HotSpot runs differ on this
        // line and nowhere else.)
        line("PreviewFeatures.isEnabled", typeOnly(() -> {
            Class<?> pf = Class.forName("jdk.internal.misc.PreviewFeatures");
            return pf.getMethod("isEnabled").invoke(null);
        }));
    }

    static String classfileVersion(String resource) {
        try (InputStream in = Object.class.getResourceAsStream(resource) != null
                ? Object.class.getResourceAsStream(resource)
                : StructuredTaskScopeProbe.class.getResourceAsStream(resource)) {
            if (in == null) {
                return "resource-absent";
            }
            DataInputStream d = new DataInputStream(in);
            int magic = d.readInt();
            int minor = d.readUnsignedShort();
            int major = d.readUnsignedShort();
            return String.format("magic=%08x major=%d minor=%d preview=%b",
                    magic, major, minor, minor == 0xFFFF);
        } catch (Throwable t) {
            return describe(t);
        }
    }

    // ------------------------------------------------------------------------
    // 4. The plainest possible use: `open()`, one Callable, `join`, read it back.
    //    Everything else in this file is a variation on these eight lines, so if
    //    this section diverges nothing below it means much.
    // ------------------------------------------------------------------------
    static void openDefault() {
        Object scope = null;
        try {
            scope = open();
            final Object s = scope;
            line("open.scopeClass", s.getClass().getName());
            line("open.isCancelled.beforeFork", valueOrThrow(() -> M_IS_CANCELLED.invoke(s)));
            Object st = fork(s, () -> 42);
            line("open.subtaskClass", st.getClass().getName());
            line("open.subtask.isSupplier", st instanceof Supplier);
            // NOT "state() before join()" — that is a race on a correct VM (a
            // trivial task usually wins) and a fixed answer on a broken one, so
            // the line would flake on HotSpot and read clean on CratonVM, which
            // is the wrong way round. `joinWaits` measures the same thing
            // deterministically.
            line("open.join.returned", valueOrThrow(() -> join(s)));
            line("open.subtask.state.afterJoin", state(st));
            line("open.subtask.get", valueOrThrow(() -> M_GET.invoke(st)));
            line("open.subtask.exception", valueOrThrow(() -> M_EXCEPTION.invoke(st)));
            line("open.isCancelled.afterJoin", valueOrThrow(() -> M_IS_CANCELLED.invoke(s)));
        } catch (Throwable t) {
            line("openDefault.threw", describe(t));
        } finally {
            closeQuietly("open", scope);
        }
    }

    // ------------------------------------------------------------------------
    // 5. THE ONE THAT MATTERS. Does `join()` actually WAIT?
    //
    //    `join()` returning before the forked task has run is not a cosmetic
    //    divergence: every value the caller then reads is a race the caller has
    //    no way to see. It is also invisible to a probe that only prints
    //    `subtask.get()` on a fast machine, because the task usually wins.
    //
    //    So this measures the ORDER directly. The task blocks on a latch the
    //    owner only releases AFTER `join()` has returned. On a VM where `join()`
    //    waits, that is a deadlock the JDK cannot reach — so the latch is
    //    released by a bounded timer instead, and the observable is the flag:
    //
    //      joinReturnedBeforeTaskStarted=false   join waited (HotSpot)
    //      joinReturnedBeforeTaskStarted=true    join did not wait
    //
    //    BOUNDED throughout: the task's own wait is capped, so neither answer
    //    can hang this section.
    // ------------------------------------------------------------------------
    static void joinWaits() {
        Object scope = null;
        try {
            AtomicInteger started = new AtomicInteger();
            AtomicInteger finished = new AtomicInteger();
            scope = open();
            Object st = fork(scope, () -> {
                started.incrementAndGet();
                // Long enough that a non-waiting join loses the race every time,
                // short enough that a waiting join is not slow. Bounded: this
                // returns on its own, it does not need anybody to release it.
                Thread.sleep(250);
                finished.incrementAndGet();
                return "done";
            });
            line("joinWaits.started.beforeJoin", started.get());
            long t0 = System.nanoTime();
            join(scope);
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            line("joinWaits.taskFinishedWhenJoinReturned", finished.get() == 1);
            line("joinWaits.joinReturnedBeforeTaskStarted", started.get() == 0);
            line("joinWaits.joinBlockedAtLeast200ms", ms >= 200);
            line("joinWaits.subtask.state.afterJoin", state(st));
            line("joinWaits.subtask.get", valueOrThrow(() -> M_GET.invoke(st)));
            // Same observable one level down, without the timing: after `close()`
            // returns, the JDK guarantees no thread of the scope is still alive.
            Thread.sleep(400);
            line("joinWaits.finished.after400msGrace", finished.get());
        } catch (Throwable t) {
            line("joinWaits.threw", describe(t));
        } finally {
            closeQuietly("joinWaits", scope);
        }
    }

    // ------------------------------------------------------------------------
    // 6-10. One section per Joiner, because the Joiner IS the policy that used
    //    to be a subclass and each one has a different `result()` type and a
    //    different cancellation rule. Printing `join()`'s return type per joiner
    //    is how a VM that wired every joiner to the same body gets caught.
    // ------------------------------------------------------------------------

    /** awaitAll: waits for every subtask, never cancels, result is Void (null). */
    static void joinerAwaitAll() {
        Object scope = null;
        try {
            scope = open(joiner("awaitAll"));
            Object ok = fork(scope, () -> "a");
            Object bad = fork(scope, () -> {
                throw new IllegalStateException("boom-b");
            });
            Object r = join(scope);
            line("awaitAll.join.result", r);
            line("awaitAll.join.resultClass", r == null ? "null" : r.getClass().getName());
            line("awaitAll.ok.state", state(ok));
            line("awaitAll.ok.get", valueOrThrow(() -> M_GET.invoke(ok)));
            line("awaitAll.bad.state", state(bad));
            line("awaitAll.bad.exception", valueOrThrow(() -> M_EXCEPTION.invoke(bad)));
            line("awaitAll.bad.get", valueOrThrow(() -> M_GET.invoke(bad)));
            final Object s = scope;
            line("awaitAll.isCancelled", valueOrThrow(() -> M_IS_CANCELLED.invoke(s)));
        } catch (Throwable t) {
            line("joinerAwaitAll.threw", describe(t));
        } finally {
            closeQuietly("awaitAll", scope);
        }
    }

    /** awaitAllSuccessfulOrThrow: the direct replacement for ShutdownOnFailure. */
    static void joinerAwaitAllSuccessful() {
        Object scope = null;
        try {
            scope = open(joiner("awaitAllSuccessfulOrThrow"));
            Object ok = fork(scope, () -> "a");
            Object r = join(scope);
            line("awaitAllSuccessful.join.result", r);
            line("awaitAllSuccessful.ok.get", valueOrThrow(() -> M_GET.invoke(ok)));
        } catch (Throwable t) {
            line("awaitAllSuccessful.happyPath.threw", describe(t));
        } finally {
            closeQuietly("awaitAllSuccessful.happy", scope);
        }
        // ...and the unhappy path, which is the whole point of this joiner: one
        // failure must cancel the scope and `join()` must throw FailedException
        // WITH the original cause attached. A VM that throws a bare
        // IllegalStateException here has lost the cause, and a caller doing
        // `catch (FailedException e) { e.getCause() }` gets nothing.
        Object scope2 = null;
        try {
            scope2 = open(joiner("awaitAllSuccessfulOrThrow"));
            Object bad = fork(scope2, () -> {
                throw new IllegalStateException("boom-c");
            });
            final Object s = scope2;
            line("awaitAllSuccessful.fail.join", thrown(() -> join(s)));
            line("awaitAllSuccessful.fail.isCancelled", valueOrThrow(() -> M_IS_CANCELLED.invoke(s)));
            line("awaitAllSuccessful.fail.bad.state", state(bad));
            line("awaitAllSuccessful.fail.bad.exception", valueOrThrow(() -> M_EXCEPTION.invoke(bad)));
        } catch (Throwable t) {
            line("awaitAllSuccessful.failPath.threw", describe(t));
        } finally {
            closeQuietly("awaitAllSuccessful.fail", scope2);
        }
    }

    /** allSuccessfulOrThrow: result is a Stream of the successful Subtasks, in fork order. */
    static void joinerAllSuccessful() {
        Object scope = null;
        try {
            scope = open(joiner("allSuccessfulOrThrow"));
            fork(scope, () -> "a");
            fork(scope, () -> "b");
            Object r = join(scope);
            line("allSuccessful.join.resultClass", r == null ? "null" : r.getClass().getName());
            line("allSuccessful.join.isStream", r instanceof Stream);
            if (r instanceof Stream<?> s) {
                // Materialise it: a Stream is where an empty answer hides most
                // easily, and `probes/JdkOnlyCollectionViewProbe` exists because
                // an empty view reads as a pass to anything that only iterates.
                line("allSuccessful.join.elements", s.map(x -> {
                    try {
                        return String.valueOf(M_GET.invoke(x));
                    } catch (Throwable t) {
                        return describe(t);
                    }
                }).collect(Collectors.joining(",")));
            }
        } catch (Throwable t) {
            line("allSuccessful.threw", describe(t));
        } finally {
            closeQuietly("allSuccessful", scope);
        }
    }

    /** anySuccessfulResultOrThrow: the direct replacement for ShutdownOnSuccess. */
    static void joinerAnySuccessful() {
        Object scope = null;
        try {
            scope = open(joiner("anySuccessfulResultOrThrow"));
            fork(scope, () -> {
                Thread.sleep(200);
                return "slow";
            });
            fork(scope, () -> "fast");
            Object r = join(scope);
            line("anySuccessful.join.result", r);
            line("anySuccessful.join.resultClass", r == null ? "null" : r.getClass().getName());
            final Object s = scope;
            line("anySuccessful.isCancelled", valueOrThrow(() -> M_IS_CANCELLED.invoke(s)));
        } catch (Throwable t) {
            line("anySuccessful.threw", describe(t));
        } finally {
            closeQuietly("anySuccessful", scope);
        }
        // All subtasks fail -> join throws, carrying one of the causes.
        Object scope2 = null;
        try {
            scope2 = open(joiner("anySuccessfulResultOrThrow"));
            fork(scope2, () -> {
                throw new IllegalStateException("boom-d");
            });
            final Object s = scope2;
            line("anySuccessful.allFail.join", thrown(() -> join(s)));
        } catch (Throwable t) {
            line("anySuccessful.allFail.threw", describe(t));
        } finally {
            closeQuietly("anySuccessful.allFail", scope2);
        }
    }

    /** allUntil(Predicate): the general form; the predicate decides when to cancel. */
    static void joinerAllUntil() {
        Object scope = null;
        try {
            AtomicInteger asked = new AtomicInteger();
            Predicate<Object> stopOnFirst = st -> {
                asked.incrementAndGet();
                return true;
            };
            Object j = JOINER.getMethod("allUntil", Predicate.class).invoke(null, stopOnFirst);
            scope = open(j);
            fork(scope, () -> "a");
            fork(scope, () -> "b");
            Object r = join(scope);
            line("allUntil.predicate.invocations", asked.get());
            line("allUntil.join.resultClass", r == null ? "null" : r.getClass().getName());
            if (r instanceof Stream<?> s) {
                line("allUntil.join.elementStates",
                        s.map(StructuredTaskScopeProbe::state).collect(Collectors.joining(",")));
            }
            final Object sc = scope;
            line("allUntil.isCancelled", valueOrThrow(() -> M_IS_CANCELLED.invoke(sc)));
        } catch (Throwable t) {
            line("allUntil.threw", describe(t));
        } finally {
            closeQuietly("allUntil", scope);
        }
    }

    // ------------------------------------------------------------------------
    // 11. `fork(Runnable)` is a JEP 505 addition — JDK 21 had only
    //     `fork(Callable)`. Its Subtask's `get()` answers null on success, which
    //     is the one place in this API where null is a RESULT and not an
    //     absence, so it is worth its own lines.
    // ------------------------------------------------------------------------
    static void forkRunnable() {
        Object scope = null;
        try {
            AtomicInteger ran = new AtomicInteger();
            scope = open();
            Object st = M_FORK_RUNNABLE.invoke(scope, (Runnable) ran::incrementAndGet);
            join(scope);
            line("forkRunnable.ran", ran.get());
            line("forkRunnable.state", state(st));
            line("forkRunnable.get", valueOrThrow(() -> M_GET.invoke(st)));
            line("forkRunnable.getIsNull", valueOrThrow(() -> M_GET.invoke(st) == null));
        } catch (Throwable t) {
            line("forkRunnable.threw", describe(t));
        } finally {
            closeQuietly("forkRunnable", scope);
        }
    }

    // ------------------------------------------------------------------------
    // 12. `open(Joiner, Function<Configuration,Configuration>)`. The Function is
    //     APPLIED by the JDK, so this also measures whether the VM calls back
    //     into user bytecode at all on this path — a native that ignores the
    //     Function reports `configFunction.applied=0` and every wither below it
    //     is meaningless.
    // ------------------------------------------------------------------------
    static void configuration() {
        Object scope = null;
        try {
            AtomicInteger applied = new AtomicInteger();
            AtomicReference<String> cfgClass = new AtomicReference<>("never-applied");
            AtomicInteger threadsMade = new AtomicInteger();
            ThreadFactory tf = r -> {
                threadsMade.incrementAndGet();
                Thread t = new Thread(r);
                t.setDaemon(true);
                return t;
            };
            Function<Object, Object> f = cfg -> {
                applied.incrementAndGet();
                cfgClass.set(cfg.getClass().getName());
                try {
                    Object withName = CONFIGURATION.getMethod("withName", String.class)
                            .invoke(cfg, "probe-scope");
                    line("configuration.withName.returnsNewInstance", withName != cfg);
                    return CONFIGURATION.getMethod("withThreadFactory", ThreadFactory.class)
                            .invoke(withName, tf);
                } catch (Throwable t) {
                    line("configuration.wither.threw", describe(t));
                    return cfg;
                }
            };
            scope = open(joiner("awaitAll"), f);
            line("configuration.applied", applied.get());
            line("configuration.configClass", cfgClass.get());
            line("configuration.scope.toString", scope.toString());
            Object st = fork(scope, () -> Thread.currentThread().isVirtual() ? "virtual" : "platform");
            join(scope);
            line("configuration.threadFactory.used", threadsMade.get());
            line("configuration.subtask.get", valueOrThrow(() -> M_GET.invoke(st)));
            // ...and the default, for contrast: JEP 505's default Configuration
            // is `Thread.ofVirtual().factory()`.
            Object plain = open();
            Object st2 = fork(plain, () -> Thread.currentThread().isVirtual() ? "virtual" : "platform");
            join(plain);
            line("configuration.default.subtaskThreadKind", valueOrThrow(() -> M_GET.invoke(st2)));
            closeQuietly("configuration.default", plain);
        } catch (Throwable t) {
            line("configuration.threw", describe(t));
        } finally {
            closeQuietly("configuration", scope);
        }
    }

    // ------------------------------------------------------------------------
    // 13. `withTimeout`. The timeout cancels the scope and `join()` throws
    //     StructuredTaskScope$TimeoutException. BOUNDED by construction: the
    //     task sleeps well past the timeout, so a VM that ignores the timeout
    //     reports a normal join rather than hanging.
    // ------------------------------------------------------------------------
    static void timeout() {
        Object scope = null;
        try {
            Function<Object, Object> f = cfg -> {
                try {
                    return CONFIGURATION.getMethod("withTimeout", Duration.class)
                            .invoke(cfg, Duration.ofMillis(150));
                } catch (Throwable t) {
                    line("timeout.wither.threw", describe(t));
                    return cfg;
                }
            };
            scope = open(joiner("awaitAll"), f);
            Object st = fork(scope, () -> {
                Thread.sleep(3000);
                return "never";
            });
            final Object s = scope;
            long t0 = System.nanoTime();
            line("timeout.join", thrown(() -> join(s)));
            line("timeout.join.under1s", (System.nanoTime() - t0) / 1_000_000L < 1000);
            line("timeout.isCancelled", valueOrThrow(() -> M_IS_CANCELLED.invoke(s)));
            line("timeout.subtask.state", state(st));
        } catch (Throwable t) {
            line("timeout.threw", describe(t));
        } finally {
            closeQuietly("timeout", scope);
        }
    }

    // ------------------------------------------------------------------------
    // 14. The state machine. JEP 505 specifies exactly when each call is legal,
    //     and every one of these is a THROW that a permissive implementation
    //     turns into a silent success — the "suite that only asserts the
    //     positive" shape. Each line prints the thrown type AND message.
    // ------------------------------------------------------------------------
    static void stateMachine() {
        // get() before join()
        Object s1 = null;
        try {
            s1 = open();
            Object st = fork(s1, () -> "x");
            line("state.getBeforeJoin", thrown(() -> M_GET.invoke(st)));
            line("state.exceptionBeforeJoin", thrown(() -> M_EXCEPTION.invoke(st)));
            // `state()` alone is deliberately NOT printed here: unlike `get()`
            // and `exception()` it does not consult the owner-joined rule, so
            // before `join()` its answer is whatever the race decided. See the
            // note in `openDefault`.
            join(s1);
        } catch (Throwable t) {
            line("state.getBeforeJoin.setup", describe(t));
        } finally {
            closeQuietly("state.getBeforeJoin", s1);
        }
        // join() twice, and fork() after join()
        Object s2 = null;
        try {
            s2 = open();
            fork(s2, () -> "x");
            join(s2);
            final Object s = s2;
            line("state.joinTwice", thrown(() -> join(s)));
            line("state.forkAfterJoin", thrown(() -> fork(s, () -> "y")));
        } catch (Throwable t) {
            line("state.joinTwice.setup", describe(t));
        } finally {
            closeQuietly("state.joinTwice", s2);
        }
        // fork() then close() with no join() — JEP 505 mandates a throw from
        // close(), which is how the API keeps "structured" from being optional.
        Object s3 = null;
        try {
            s3 = open();
            fork(s3, () -> "x");
            final Object s = s3;
            line("state.closeWithoutJoin", thrown(() -> close(s)));
            // close() is idempotent AFTER it has thrown once; the scope is shut.
            line("state.closeAgain", thrown(() -> close(s)));
        } catch (Throwable t) {
            line("state.closeWithoutJoin.setup", describe(t));
        }
        // Everything after close()
        Object s4 = null;
        try {
            s4 = open();
            close(s4);
            final Object s = s4;
            line("state.forkAfterClose", thrown(() -> fork(s, () -> "y")));
            line("state.joinAfterClose", thrown(() -> join(s)));
            line("state.isCancelledAfterClose", valueOrThrow(() -> M_IS_CANCELLED.invoke(s)));
        } catch (Throwable t) {
            line("state.afterClose.setup", describe(t));
        }
        // close() with no fork and no join at all is legal and silent.
        Object s5 = null;
        try {
            s5 = open();
            final Object s = s5;
            line("state.closeNeverForked", thrown(() -> close(s)));
        } catch (Throwable t) {
            line("state.closeNeverForked.setup", describe(t));
        }
    }

    // ------------------------------------------------------------------------
    // 15. Owner-thread confinement. `fork`/`join`/`close` from a thread that did
    //     not open the scope must throw WrongThreadException. This is the rule
    //     that makes the API structured, and it is entirely invisible to any
    //     single-threaded probe.
    // ------------------------------------------------------------------------
    static void ownerThread() {
        Object scope = null;
        try {
            scope = open();
            final Object s = scope;
            AtomicReference<String> forkFromOther = new AtomicReference<>("thread-never-ran");
            AtomicReference<String> joinFromOther = new AtomicReference<>("thread-never-ran");
            AtomicReference<String> closeFromOther = new AtomicReference<>("thread-never-ran");
            Thread other = new Thread(() -> {
                forkFromOther.set(thrown(() -> fork(s, () -> "z")));
                joinFromOther.set(thrown(() -> join(s)));
                closeFromOther.set(thrown(() -> close(s)));
            }, "probe-nonowner");
            other.setDaemon(true);
            other.start();
            other.join(4000);
            line("ownerThread.otherThreadFinished", !other.isAlive());
            line("ownerThread.forkFromOther", forkFromOther.get());
            line("ownerThread.joinFromOther", joinFromOther.get());
            line("ownerThread.closeFromOther", closeFromOther.get());
        } catch (Throwable t) {
            line("ownerThread.threw", describe(t));
        } finally {
            closeQuietly("ownerThread", scope);
        }
    }

    // ------------------------------------------------------------------------
    // 16. What a Subtask IS. It is a `Supplier`, its `toString` encodes its
    //     state, and — the part a re-implementation gets wrong — two forks must
    //     hand back two DISTINCT carriers. A VM that caches one Subtask per
    //     scope passes every section above and fails here.
    // ------------------------------------------------------------------------
    static void subtaskCarrier() {
        Object scope = null;
        try {
            scope = open();
            Object a = fork(scope, () -> "a");
            Object b = fork(scope, () -> "b");
            line("subtask.distinctInstances", a != b);
            join(scope);
            line("subtask.a.get", valueOrThrow(() -> M_GET.invoke(a)));
            line("subtask.b.get", valueOrThrow(() -> M_GET.invoke(b)));
            line("subtask.a.viaSupplier", valueOrThrow(() -> ((Supplier<?>) a).get()));
            // toString is specified to encode the state; printing it catches a
            // carrier that renders as a bare identity hash.
            line("subtask.a.toStringHasState", String.valueOf(a).contains("["));
            line("subtask.a.toStringSuffix", suffixFrom(String.valueOf(a)));
            line("subtask.state.enumIdentity", valueOrThrow(
                    () -> M_STATE.invoke(a) == M_STATE.invoke(b)));
            line("subtask.state.declaringClass", valueOrThrow(
                    () -> M_STATE.invoke(a).getClass().getName()));
        } catch (Throwable t) {
            line("subtaskCarrier.threw", describe(t));
        } finally {
            closeQuietly("subtaskCarrier", scope);
        }
    }

    /** The bracketed tail of a Subtask's toString — the identity hash before it is not diffable. */
    static String suffixFrom(String s) {
        int i = s.indexOf('[');
        return i < 0 ? "no-bracket" : s.substring(i);
    }
}
