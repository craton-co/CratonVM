import java.io.ByteArrayOutputStream;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.List;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogManager;
import java.util.logging.LogRecord;
import java.util.logging.Logger;
import java.util.logging.SimpleFormatter;
import java.util.logging.StreamHandler;

/**
 * `java.util.logging` — the vector whose ABSENCE let a regression ship.
 *
 * On 2026-08-11 a wave retired 84 `java/util/logging/` shadows (a `Bridge`
 * native registered over a real class's real bytecode gets re-tagged
 * `SyntheticStub`, which `--jdk-only` refuses, so the bytecode runs). That
 * retirement was recorded as "measured verdict-neutral against the strict
 * corpus" and became the campaign's cited precedent for every later one. It
 * was verdict-neutral because NOTHING TESTED IT: a grep over all 57 vectors
 * for `java.util.logging` matched zero files. What it actually did was make
 * `Logger.getLogger("x")` — the first call any JUL user makes — throw
 * `NullPointerException: … because the return value of
 * "java.util.logging.LogManager.getSystemContext()" is null`, because the
 * `LogManager` singleton was allocated and never constructed.
 *
 * So the point of this file is not "JUL works". It is that the retired
 * surface is exercised through the REAL objects, with the answers read back,
 * so a shadow retirement that leaves the state fake fails here instead of
 * being licensed by a green run.
 *
 * Two hazards this vector is written against, both earned in this campaign:
 *
 * 1. **A check that cannot fail is worse than no check.** Every assertion
 *    below is on a VALUE that came back — a captured `LogRecord`, a handler
 *    array's length, formatted bytes, an object identity — never on "no
 *    exception was thrown". Breaking the VM breaks these.
 * 2. **This subsystem fails SILENTLY.** `java.io.PrintStream.writeln`'s own
 *    exception table catches the `IOException` that `ensureOpen()` raises on
 *    a stream whose `out` is null and sets `trouble = true`, so a retired
 *    `PrintStream` shadow over `System.out` DISCARDS output and exits 0. A
 *    logging vector that only asserted "the call returned" would read green
 *    against a VM that logged nothing. {@link #formattedOutputIsRealBytes()}
 *    is the answer: it captures a `StreamHandler`'s bytes and asserts on
 *    their content.
 *
 * Both CratonVM modes must match HotSpot here. Unlike RJdkStrict this is not
 * a mode-divergent vector: `--real-jdk` reaches the same surface through the
 * natives and `--jdk-only` reaches it through the bytecode, and HotSpot's
 * answer is the answer in both.
 *
 * Every expectation below was measured on Temurin 25.0.3+9 first.
 */
public class RJdkLogging {
    static int checks;

    /**
     * The source pair is a STATE MACHINE, not two fields, and the machine is
     * `needToInferCaller`. JDK 25:
     *
     *   getSourceClassName()  { if (needToInferCaller) inferCaller(); return sourceClassName; }
     *   setSourceClassName(s) { this.sourceClassName = s; needToInferCaller = false; }
     *   inferCaller()         { needToInferCaller = false; CallerFinder.get().ifPresent(...); }
     *
     * Three properties fall out of that, and each is a defect some arrangement
     * of CratonVM's four shadow accessors produces:
     *
     *  * an explicit set must NOT be overwritten by inference on the next read
     *    — what a GETTER-ONLY retirement breaks, because the real getter would
     *    then honour a flag the surviving shadow setter never cleared;
     *  * setting ONE half clears the flag for BOTH, so the other half stays
     *    null rather than being inferred — a setter that forgets the clear
     *    fills it in, and a weaker check would call that a pass;
     *  * a record built by hand, with no `java.util.logging.Logger` frame on
     *    the stack, infers NOTHING: CallerFinder's latch never trips. A VM that
     *    answers a class name here is inferring from a stack HotSpot refuses to.
     *
     * docs/known-issues/jdk-only/W7-56-infercaller-strict.md
     */
    static void sourcePairStateMachine() {
        LogRecord explicit = new LogRecord(Level.INFO, "explicit");
        explicit.setSourceClassName("A_CLASS");
        explicit.setSourceMethodName("a_method");
        check("A_CLASS".equals(explicit.getSourceClassName()),
                "an explicit source class must survive the next read, not be overwritten by"
                        + " inference; got " + explicit.getSourceClassName());
        check("a_method".equals(explicit.getSourceMethodName()),
                "an explicit source method must survive the next read; got "
                        + explicit.getSourceMethodName());

        LogRecord halfSet = new LogRecord(Level.INFO, "half");
        halfSet.setSourceClassName("B_CLASS");
        check("B_CLASS".equals(halfSet.getSourceClassName()),
                "got " + halfSet.getSourceClassName());
        check(halfSet.getSourceMethodName() == null,
                "setting the class alone clears needToInferCaller, so the METHOD must stay null"
                        + " rather than be inferred; got " + halfSet.getSourceMethodName());

        LogRecord bare = new LogRecord(Level.INFO, "bare");
        check(bare.getSourceClassName() == null,
                "no java.util.logging.Logger frame is on this stack, so CallerFinder's latch"
                        + " never trips and the pair must stay null; got "
                        + bare.getSourceClassName());
        check(bare.getSourceMethodName() == null,
                "and the method half likewise; got " + bare.getSourceMethodName());
        bare.setSourceClassName("LATE");
        check("LATE".equals(bare.getSourceClassName()),
                "a set AFTER a failed inference must still stick; got "
                        + bare.getSourceClassName());

        System.out.println("CK RJdkLogging sourcePair=" + explicit.getSourceClassName() + "/"
                + explicit.getSourceMethodName() + " halfSetMethod=" + halfSet.getSourceMethodName()
                + " bare=" + bare.getSourceClassName());
    }

    /**
     * #59 at RECORD level rather than through the formatter, so an inference
     * regression and a formatting regression cannot be confused for each other.
     *
     * Read INSIDE `publish`, deliberately. HotSpot's inference is LAZY and
     * fails once its frames are gone, so the pair read AFTER the log call has
     * returned is a known, measured divergence and is not asserted here.
     */
    static void publishedRecordCarriesItsCaller() {
        Logger l = logger("rjdklogging.srcpair");
        l.setUseParentHandlers(false);
        l.setLevel(Level.ALL);
        final String[] seen = new String[2];
        Handler h = new Handler() {
            @Override public void publish(LogRecord r) {
                seen[0] = r.getSourceClassName();
                seen[1] = r.getSourceMethodName();
            }
            @Override public void flush() { }
            @Override public void close() { }
        };
        h.setLevel(Level.ALL);
        l.addHandler(h);
        l.warning("SRC-PAIR-MARK");
        l.removeHandler(h);

        check("RJdkLogging".equals(seen[0]),
                "a record published through a Logger must name the CALLING class, read during"
                        + " publish; got " + seen[0]);
        check("publishedRecordCarriesItsCaller".equals(seen[1]),
                "...and the calling method; got " + seen[1]);
        System.out.println("CK RJdkLogging publishedSource=" + seen[0] + "/" + seen[1]);
    }

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Loggers are held through weak references by their `LoggerContext`, so a
     * logger only reachable from a local would be collectible between the call
     * that created it and the call that is supposed to find it again — and the
     * identity checks below would then be measuring the GC, not the registry.
     * Every logger this vector creates is parked here for the run.
     */
    static final List<Logger> STRONG = new ArrayList<>();

    static Logger logger(String name) {
        Logger l = Logger.getLogger(name);
        STRONG.add(l);
        return l;
    }

    /** Captures whole `LogRecord`s so assertions can read the record back. */
    static final class Capture extends Handler {
        final List<LogRecord> records = new ArrayList<>();
        int flushes;
        int closes;

        @Override
        public void publish(LogRecord r) {
            records.add(r);
        }

        @Override
        public void flush() {
            flushes++;
        }

        @Override
        public void close() {
            closes++;
        }

        List<String> rendered() {
            List<String> out = new ArrayList<>();
            for (LogRecord r : records) {
                out.add(r.getLevel().getName() + ":" + r.getMessage());
            }
            return out;
        }
    }

    /** A logger with exactly one Capture attached and no parent handlers. */
    static Capture attach(Logger l) {
        Capture c = new Capture();
        l.setUseParentHandlers(false);
        l.addHandler(c);
        return c;
    }

    /**
     * `LogManager.getLogManager()` is the object whose null `systemContext`
     * was the regression. Its identity and its registry are both observable
     * without reflection, so this asserts on them rather than on the fields.
     */
    static void logManagerSingleton() {
        LogManager a = LogManager.getLogManager();
        check(a != null, "LogManager.getLogManager() returned null");
        LogManager b = LogManager.getLogManager();
        check(a == b, "getLogManager() must return the same singleton every call");
        check(a instanceof LogManager, "the singleton must be a LogManager: " + a.getClass());

        // The registry round-trip. This is the getSystemContext()/userContext
        // path stated as an observable: `Logger.getLogger` demands the logger
        // through a LoggerContext and `LogManager.getLogger` reads the same
        // map, so agreement here means the contexts are real and are the SAME
        // context. A fabricated manager with a Rust-side name map can return a
        // logger from `getLogger` and still fail this, because the object it
        // returns is not the one `Logger.getLogger` handed out.
        Logger demanded = logger("rjdklogging.registry.one");
        Logger viaManager = a.getLogger("rjdklogging.registry.one");
        check(viaManager != null,
                "LogManager.getLogger must find a logger Logger.getLogger created");
        check(viaManager == demanded,
                "LogManager.getLogger must return the SAME instance Logger.getLogger did");

        // NOT ASSERTED, deliberately, and this is the one omission in the file:
        // HotSpot returns null from `LogManager.getLogger` for a name nobody
        // demanded, and CratonVM's `Compatible` mode returns a fresh Logger —
        // measured 2026-08-11 on `target/release/cratonvm.exe`, HotSpot `null`
        // vs `java.util.logging.Logger`. That is a third, pre-existing defect
        // with its own cause (`LogManager.getLogger(String)` is a registered
        // Intrinsic that demand-creates through a Rust-side name registry) and
        // its own blast radius (the JULI/Tomcat shims are built on that
        // demand-creation). Asserting it here would make this vector red for a
        // reason it is not gating, so it is written down in
        // docs/known-issues/jdk-only/W7-25-jul-getlogger-regression.md instead
        // of being asserted or forgotten.

        List<String> names = new ArrayList<>();
        for (Enumeration<String> e = a.getLoggerNames(); e.hasMoreElements();) {
            names.add(e.nextElement());
        }
        check(names.contains("rjdklogging.registry.one"),
                "getLoggerNames() must list a demanded logger; got " + names.size() + " names");
        System.out.println("CK RJdkLogging singleton=" + (a == b)
                + " registryRoundTrip=" + (viaManager == demanded)
                + " loggerNames=" + names.size());
    }

    /**
     * `Logger.getLogger` itself: the call the regression broke. Identity,
     * name, and the parent chain the JDK builds out of the dotted name — a
     * chain a per-name allocator that never links parents cannot produce.
     */
    static void getLoggerIdentityAndParents() {
        Logger a = logger("rjdklogging.tree.alpha");
        check(a != null, "Logger.getLogger returned null");
        check("rjdklogging.tree.alpha".equals(a.getName()),
                "getName() must echo the requested name, got " + a.getName());
        check(logger("rjdklogging.tree.alpha") == a,
                "getLogger must return the same instance for the same name");
        check(logger("rjdklogging.tree.beta") != a,
                "getLogger must return distinct instances for distinct names");

        // Walk to the root. The chain must terminate, and it must terminate on
        // the root logger — whose name is the empty string on HotSpot.
        int hops = 0;
        Logger walk = a;
        while (walk.getParent() != null && hops < 32) {
            walk = walk.getParent();
            hops++;
        }
        check(hops > 0, "a dotted logger must have at least one parent");
        check(hops < 32, "the parent chain must terminate, not cycle");
        check("".equals(walk.getName()),
                "the parent chain must end at the root logger; ended at " + walk.getName());

        Logger global = Logger.getGlobal();
        check(global != null, "Logger.getGlobal() returned null");
        check("global".equals(global.getName()),
                "the global logger is named 'global', got " + global.getName());
        System.out.println("CK RJdkLogging parentHops=" + hops + " rootName='" + walk.getName()
                + "' global=" + global.getName());
    }

    /**
     * Level filtering, asserted on WHICH RECORDS ARRIVED. `isLoggable` is
     * checked too, but on its own it is a predicate agreeing with itself —
     * the delivered set is what proves the predicate is the one the log call
     * consults.
     */
    static void levelFiltering() {
        Logger l = logger("rjdklogging.levels");
        Capture c = attach(l);

        l.setLevel(Level.WARNING);
        check(Level.WARNING.equals(l.getLevel()),
                "getLevel must echo setLevel, got " + l.getLevel());
        check(!l.isLoggable(Level.INFO), "INFO must not be loggable at WARNING");
        check(l.isLoggable(Level.WARNING), "WARNING must be loggable at WARNING");
        check(l.isLoggable(Level.SEVERE), "SEVERE must be loggable at WARNING");

        l.info("dropped-info");
        l.warning("kept-warning");
        l.severe("kept-severe");
        l.fine("dropped-fine");
        check(c.rendered().equals(java.util.Arrays.asList(
                "WARNING:kept-warning", "SEVERE:kept-severe")),
                "level WARNING must admit exactly WARNING+SEVERE; got " + c.rendered());

        // Widening the level must admit the finer records — the same logger,
        // the same handler, a different answer. A no-op setLevel fails here.
        c.records.clear();
        l.setLevel(Level.FINE);
        check(l.isLoggable(Level.FINE), "FINE must be loggable at FINE");
        check(!l.isLoggable(Level.FINEST), "FINEST must not be loggable at FINE");
        l.fine("kept-fine");
        l.finest("dropped-finest");
        l.info("kept-info");
        check(c.rendered().equals(java.util.Arrays.asList("FINE:kept-fine", "INFO:kept-info")),
                "level FINE must admit FINE and INFO but not FINEST; got " + c.rendered());

        // OFF admits nothing at all.
        c.records.clear();
        l.setLevel(Level.OFF);
        l.severe("dropped-at-off");
        check(c.records.isEmpty(), "Level.OFF must admit nothing; got " + c.rendered());
        l.setLevel(Level.INFO);
        System.out.println("CK RJdkLogging levelFiltering=ok records=" + c.records.size());
    }

    /**
     * The handler chain as a mutable, readable structure: added handlers are
     * visible through `getHandlers`, removed ones are gone, and a removed
     * handler stops receiving. `getHandlers` returning a fresh empty array
     * forever is a real failure shape here and each of these three catches it.
     */
    static void handlerChain() {
        Logger l = logger("rjdklogging.handlers");
        l.setUseParentHandlers(false);
        check(l.getHandlers().length == 0,
                "a fresh logger has no handlers; got " + l.getHandlers().length);

        Capture c = new Capture();
        l.addHandler(c);
        Handler[] after = l.getHandlers();
        check(after.length == 1, "one handler added, " + after.length + " reported");
        check(after[0] == c, "getHandlers must report the handler instance that was added");

        l.info("through-handler");
        check(c.records.size() == 1, "the installed handler must receive; got " + c.records.size());

        LogRecord r = c.records.get(0);
        check(Level.INFO.equals(r.getLevel()), "record level, got " + r.getLevel());
        check("through-handler".equals(r.getMessage()), "record message, got " + r.getMessage());
        check("rjdklogging.handlers".equals(r.getLoggerName()),
                "record must carry the logger name, got " + r.getLoggerName());

        l.removeHandler(c);
        check(l.getHandlers().length == 0,
                "removeHandler must unregister; " + l.getHandlers().length + " left");
        l.info("after-removal");
        check(c.records.size() == 1,
                "a removed handler must stop receiving; got " + c.records.size());
        System.out.println("CK RJdkLogging handlerChain=add,report,receive,remove records="
                + c.records.size());
    }

    /**
     * `useParentHandlers` — the ancestor walk. A child with no handler of its
     * own must reach its parent's, and must stop reaching it when the flag is
     * cleared. This is the one place the parent LINK built in
     * {@link #getLoggerIdentityAndParents()} is load-bearing rather than just
     * inspectable.
     */
    static void parentHandlerDelivery() {
        Logger parent = logger("rjdklogging.inherit");
        Capture pc = attach(parent);
        Logger child = logger("rjdklogging.inherit.child");
        check(child.getHandlers().length == 0, "the child must have no handler of its own");
        check(child.getUseParentHandlers(), "useParentHandlers defaults to true");

        child.info("up-to-parent");
        check(pc.rendered().equals(Collections.singletonList("INFO:up-to-parent")),
                "a child record must reach the parent's handler; got " + pc.rendered());
        check("rjdklogging.inherit.child".equals(pc.records.get(0).getLoggerName()),
                "the record must keep the CHILD's name, got " + pc.records.get(0).getLoggerName());

        child.setUseParentHandlers(false);
        check(!child.getUseParentHandlers(), "setUseParentHandlers(false) must stick");
        child.info("not-up-to-parent");
        check(pc.records.size() == 1,
                "useParentHandlers=false must stop the walk; got " + pc.rendered());
        System.out.println("CK RJdkLogging parentDelivery=" + pc.rendered());
    }

    /**
     * `log(Level, Supplier)` and its siblings.
     *
     * This overload carries a SECOND, independent defect, pre-existing and
     * live in `Compatible` mode: CratonVM's native resolved the supplier and
     * wrote the console sink directly without fanning the record out to the
     * logger's handlers, so an application handler saw seven of the eight
     * `log` overloads. Measured 2026-08-11 —
     *
     *   HotSpot         [INFO:i, WARNING:w, SEVERE:s, FINE:f, INFO:L, INFO:sup]
     *   CratonVM compat [INFO:i, WARNING:w, SEVERE:s, FINE:f, INFO:L]
     *
     * — so the supplier row is what this method exists to pin. The
     * NOT-EVALUATED check is the other half and is the stronger one: the whole
     * reason the overload exists is that a filtered-out supplier must never
     * run, and a native that resolves the supplier before consulting the level
     * passes every "was it delivered" assertion while failing this.
     */
    static void supplierOverloads() {
        Logger l = logger("rjdklogging.supplier");
        Capture c = attach(l);
        l.setLevel(Level.FINE);

        final int[] calls = { 0 };
        l.log(Level.INFO, () -> {
            calls[0]++;
            return "sup-info";
        });
        check(calls[0] == 1, "an admitted supplier must be evaluated exactly once, got " + calls[0]);
        check(c.rendered().equals(Collections.singletonList("INFO:sup-info")),
                "log(Level, Supplier) must reach the handler; got " + c.rendered());

        // Filtered out: neither delivered NOR evaluated.
        l.log(Level.FINEST, () -> {
            calls[0]++;
            return "sup-finest";
        });
        check(calls[0] == 1, "a filtered-out supplier must NOT be evaluated, got " + calls[0]);
        check(c.records.size() == 1, "a filtered-out supplier record must not be delivered");

        // The level-named supplier convenience methods take the same path.
        c.records.clear();
        l.info(() -> "conv-info");
        l.warning(() -> "conv-warning");
        l.fine(() -> "conv-fine");
        l.finest(() -> "conv-finest-dropped");
        check(c.rendered().equals(java.util.Arrays.asList(
                "INFO:conv-info", "WARNING:conv-warning", "FINE:conv-fine")),
                "the Supplier convenience overloads must filter and deliver; got " + c.rendered());

        // The throwable-carrying supplier overload must carry the throwable.
        c.records.clear();
        Exception boom = new IllegalStateException("supplier-thrown");
        l.log(Level.SEVERE, boom, () -> "sup-with-throwable");
        check(c.records.size() == 1, "log(Level, Throwable, Supplier) must deliver");
        check(c.records.get(0).getThrown() == boom,
                "the record must carry the throwable that was passed");
        check("sup-with-throwable".equals(c.records.get(0).getMessage()),
                "the record must carry the resolved supplier text, got "
                        + c.records.get(0).getMessage());
        System.out.println("CK RJdkLogging supplier=evaluated:" + calls[0]
                + " thrown=" + c.records.get(0).getThrown().getClass().getSimpleName());
    }

    /**
     * `log(LogRecord)` and the record payloads a Formatter reads. A record
     * built by the caller must arrive as ITSELF, with its parameters intact
     * and its message still the RAW pattern — HotSpot substitutes `{n}` in the
     * Formatter, never in the record.
     */
    static void recordPayloads() {
        Logger l = logger("rjdklogging.records");
        Capture c = attach(l);
        l.setLevel(Level.ALL);

        LogRecord mine = new LogRecord(Level.WARNING, "hand-built");
        mine.setLoggerName("rjdklogging.records");
        l.log(mine);
        check(c.records.size() == 1, "log(LogRecord) must deliver; got " + c.records.size());
        check(c.records.get(0) == mine,
                "log(LogRecord) must deliver the SAME record object, not a copy of its text");

        c.records.clear();
        l.log(Level.INFO, "one={0} two={1}", new Object[] { "A", "B" });
        check(c.records.size() == 1, "the parameterized overload must deliver");
        LogRecord r = c.records.get(0);
        check("one={0} two={1}".equals(r.getMessage()),
                "the record keeps the RAW pattern; got " + r.getMessage());
        check(r.getParameters() != null && r.getParameters().length == 2,
                "the record must carry both parameters");
        check("A".equals(r.getParameters()[0]) && "B".equals(r.getParameters()[1]),
                "the parameters must be the values that were passed");

        // ...and the Formatter is where they get substituted.
        String formatted = new SimpleFormatter().formatMessage(r);
        check("one=A two=B".equals(formatted),
                "Formatter.formatMessage must substitute; got " + formatted);

        // `log(LogRecord)` is LEVEL-GATED. JDK 25's body opens with
        // `if (!isLoggable(record.getLevel())) return;`, and this was the one
        // member of the eight-overload `log` family that published
        // unconditionally — so a record below the logger's threshold arrived
        // anyway, and a caller who set a level to suppress diagnostics got
        // them. The section above cannot see it: it runs at `Level.ALL`, where
        // the gate admits everything, which is exactly how a missing gate
        // hides behind a passing delivery check.
        //
        // Both polarities, in that order: a native that dropped every record
        // would satisfy the negative half on its own, and the positive half
        // after it is what stops this check from being satisfied by silence.
        c.records.clear();
        l.setLevel(Level.WARNING);
        l.log(new LogRecord(Level.FINEST, "RECORD-GATE-DROPPED"));
        check(c.records.isEmpty(),
                "log(LogRecord) must drop a record below the logger's level; got " + c.rendered());
        l.log(new LogRecord(Level.SEVERE, "RECORD-GATE-ADMITTED"));
        check(c.rendered().equals(Collections.singletonList("SEVERE:RECORD-GATE-ADMITTED")),
                "log(LogRecord) must deliver a record at or above the level; got " + c.rendered());
        l.setLevel(Level.ALL);

        System.out.println("CK RJdkLogging recordPayloads formatted='" + formatted
                + "' recordGate=ok");
    }

    /**
     * THE ANTI-SILENCE CHECK. Everything above reads a `LogRecord` back out of
     * a handler written in this file, which proves the dispatch but not that
     * anything was ever WRITTEN. This one drives a real `StreamHandler` over a
     * real `SimpleFormatter` into a `ByteArrayOutputStream` and asserts on the
     * bytes — the shape that catches a sink which accepts, formats and then
     * discards, which is exactly what a retired `PrintStream` shadow over
     * `System.out` does (its `writeln` catches its own `IOException` and sets
     * `trouble`, so nothing is printed and the exit code stays 0).
     */
    static void formattedOutputIsRealBytes() throws Exception {
        Logger l = logger("rjdklogging.stream");
        l.setUseParentHandlers(false);
        l.setLevel(Level.ALL);
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        StreamHandler sh = new StreamHandler(sink, new SimpleFormatter());
        sh.setLevel(Level.ALL);
        l.addHandler(sh);

        l.warning("BYTES-MARK-ONE");
        l.log(Level.INFO, () -> "BYTES-MARK-TWO");
        sh.flush();

        String text = sink.toString("UTF-8");
        check(!text.isEmpty(), "the StreamHandler wrote NOTHING — the sink is silent");
        check(text.contains("BYTES-MARK-ONE"),
                "the formatted output must contain the logged message; got [" + text + "]");
        check(text.contains("BYTES-MARK-TWO"),
                "the supplier overload must reach the stream too; got [" + text + "]");
        // NOTE, and it is a real hole rather than an oversight: this is a
        // `contains` over an English token, and an all-English rendering
        // satisfies it whatever the format locale is — so a VM whose locale
        // data collapsed back to `en` passes. The locale-sensitive half of
        // SimpleFormatter's default pattern is its DATE prefix
        // (`%1$tb %1$td, %1$tY … %1$Tp`), and an assertion on it would be
        // date-dependent, which the suite forbids. The locale-sensitivity
        // assertion therefore lives in `RStrings` (`DecimalFormatSymbols` vs
        // `%,.2f`), where the surface is one CratonVM implements per-locale and
        // the check is an implication rather than a pinned rendering. Do not
        // "strengthen" this line into a month name.
        //
        // That prefix is nonetheless where TWO measured defects lived, and both
        // were found by the cross-VM diff over `streamBytes` below rather than
        // by any assertion in this file — each cost exactly one character per
        // rendered line, `2M + 2H + 167` bytes total:
        //   * the MONTH (`%1$tb`) came from hard-coded English tables in
        //     `native-builtins/src/lang_string.rs`, so it rendered `Aug` where
        //     HotSpot rendered this host's locale's four-character form —
        //     W7-91-format-date-symbols-hardcoded-english.md;
        //   * the HOUR (`%1$tl`, unpadded 12-hour) came from a system default
        //     zone that was the string `"UTC"` on every host, so it rendered
        //     one digit where HotSpot rendered two —
        //     W7-92-system-timezone-answers-utc.md.
        // `defaultZoneReachesTheFormatter()` below is the durable, zone- and
        // month-agnostic cover for the second; there is no app-level witness
        // for either, which is the point being recorded here.
        check(text.contains("WARNING"),
                "SimpleFormatter must render the level name; got [" + text + "]");
        // SimpleFormatter's default pattern renders the SOURCE class and
        // method, not the logger name — measured, not assumed, and the reason
        // the first draft of this check failed on HotSpot. Inferring the
        // source pair is real work (`LogRecord` fills it by walking the stack)
        // and a record that arrived without it renders "null null" here.
        check(text.contains("RJdkLogging formattedOutputIsRealBytes"),
                "SimpleFormatter must render the inferred source class and method; got ["
                        + text + "]");
        // Two records, two rendered lines: a sink that wrote one blob for both
        // (or replayed one twice) fails here.
        int marks = (text.contains("BYTES-MARK-ONE") ? 1 : 0) + (text.contains("BYTES-MARK-TWO") ? 1 : 0);
        check(marks == 2, "both records must appear; got " + marks);

        l.removeHandler(sh);
        sh.close();
        System.out.println("CK RJdkLogging streamBytes=" + text.length() + " marks=" + marks);
    }

    /**
     * Handler-level filtering — the second gate, independent of the logger's.
     * A handler whose level rejects a record must not see it even though the
     * logger admitted it, which is the only way to tell the two gates apart.
     */
    static void handlerLevelIsASecondGate() {
        Logger l = logger("rjdklogging.handlerlevel");
        Capture c = attach(l);
        l.setLevel(Level.ALL);
        c.setLevel(Level.SEVERE);
        check(Level.SEVERE.equals(c.getLevel()),
                "Handler.getLevel must echo setLevel, got " + c.getLevel());

        l.info("admitted-by-logger-rejected-by-handler");
        l.severe("admitted-by-both");
        // StreamHandler applies its own level in publish(); a bare Handler
        // subclass does not, so assert through one that does.
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        StreamHandler sh = new StreamHandler(sink, new SimpleFormatter());
        sh.setLevel(Level.SEVERE);
        l.addHandler(sh);
        l.info("HL-INFO-MARK");
        l.severe("HL-SEVERE-MARK");
        sh.flush();
        String text = sink.toString();
        check(!text.contains("HL-INFO-MARK"),
                "a handler at SEVERE must reject an INFO record; got [" + text + "]");
        check(text.contains("HL-SEVERE-MARK"),
                "a handler at SEVERE must accept a SEVERE record; got [" + text + "]");
        l.removeHandler(sh);
        System.out.println("CK RJdkLogging handlerLevelGate=ok bytes=" + text.length());
    }

    /**
     * THE DATE PREFIX'S ZONE. `SimpleFormatter.format` renders
     * `ZonedDateTime.ofInstant(record.getInstant(), ZoneId.systemDefault())`,
     * so the clock reading in every line {@link #formattedOutputIsRealBytes()}
     * captures is the SYSTEM DEFAULT ZONE's. Until W7-92 CratonVM's system
     * default zone was UTC on every host — `TimeZone.getSystemTimeZoneID`
     * returned the literal string `"UTC"`, and in `--real-jdk` mode the
     * `TimeZone.getDefault()` native fell back to the same constant — so every
     * logged timestamp on a host that is not at UTC was off by a whole offset.
     *
     * Nothing here pins a zone, an offset, a month, a clock reading or a byte
     * count. A Java program cannot know its host's zone except through the JVM,
     * so this defect has NO app-level witness; the two quantities below are
     * therefore PRINTED for the same-session cross-VM diff, which is what
     * caught it. `getRawOffset()` is the printed one on purpose: it is a
     * per-zone constant, so it is stable across the two VMs' different start
     * instants and across a DST transition, and it is identical for every IANA
     * id that shares the host's platform zone — so it does not go red merely
     * because the two VMs spell the zone's NAME differently.
     *
     * The `check()` is an implication that holds in every zone in every month:
     * the formatter's clock reading must be the record's own instant expressed
     * in the zone the JVM itself calls default. It does NOT catch W7-92, where
     * both were UTC and agreed. It catches a HALF fix — which, with two
     * independent producers of the default zone on two dispatch routes, is the
     * likely way this regresses.
     */
    static void defaultZoneReachesTheFormatter() throws Exception {
        java.util.TimeZone def = java.util.TimeZone.getDefault();
        check(def != null, "TimeZone.getDefault() returned null");
        java.time.ZoneId zone = java.time.ZoneId.systemDefault();
        check(zone != null, "ZoneId.systemDefault() returned null");

        Logger l = logger("rjdklogging.zone");
        l.setUseParentHandlers(false);
        l.setLevel(Level.ALL);
        Capture c = new Capture();
        l.addHandler(c);
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        StreamHandler sh = new StreamHandler(sink, new SimpleFormatter());
        sh.setLevel(Level.ALL);
        l.addHandler(sh);
        // One record, two handlers: the Capture keeps the very object the
        // StreamHandler formatted, so the instant compared below is the
        // formatter's own input and not a second reading of the clock.
        l.warning("ZONE-MARK");
        sh.flush();
        String text = sink.toString("UTF-8");
        l.removeHandler(sh);
        l.removeHandler(c);
        sh.close();
        check(c.records.size() == 1, "the Capture must hold the formatted record");
        java.time.Instant when = c.records.get(0).getInstant();
        check(when != null, "LogRecord.getInstant() returned null");

        // SimpleFormatter's DEFAULT pattern only — a run that overrides it
        // through the system property or a logging.properties has a different
        // prefix and this implication does not apply to it.
        String pattern = System.getProperty("java.util.logging.SimpleFormatter.format");
        if (pattern == null) {
            pattern = LogManager.getLogManager()
                    .getProperty("java.util.logging.SimpleFormatter.format");
        }
        boolean defaultPattern = pattern == null;
        if (defaultPattern) {
            // `%1$tl:%1$tM:%1$tS` — 12-hour clock WITHOUT padding (so hour 12
            // for midnight and noon), then two padded fields.
            java.time.ZonedDateTime zdt = java.time.ZonedDateTime.ofInstant(when, zone);
            int hour12 = zdt.getHour() % 12 == 0 ? 12 : zdt.getHour() % 12;
            // Built with String.format, not concatenation: `%1$tl` and friends
            // go through java.util.Formatter's localized digits, and a locale
            // whose zero digit is not ASCII '0' would make a hand-built string
            // disagree with the formatter for a reason that is not a defect.
            String clock = String.format("%d:%02d:%02d", hour12, zdt.getMinute(), zdt.getSecond());
            check(text.contains(clock),
                    "SimpleFormatter must render the record's instant in ZoneId.systemDefault()"
                            + " (" + zone + "): expected the clock reading " + clock
                            + " in [" + text + "]");
        }

        // `ZoneId.systemDefault()` IS `TimeZone.getDefault().toZoneId()` by
        // definition, and the two reach it through different code in CratonVM,
        // so their agreement is worth reading back. Reported rather than
        // asserted, and the throw is caught: a `toZoneId()` that fails is
        // evidence for the next lane, not a reason for this vector to die
        // somewhere other than at the thing it is gating.
        String zoneAgrees;
        try {
            zoneAgrees = String.valueOf(zone.equals(def.toZoneId()));
        } catch (RuntimeException e) {
            zoneAgrees = "threw:" + e.getClass().getSimpleName();
        }
        // Printed, not asserted — see this method's doc comment. `getRawOffset`
        // is a per-zone constant in milliseconds, so this line is identical on
        // both VMs whenever they agree about the host's zone and differs by the
        // whole offset when they do not.
        System.out.println("CK RJdkLogging defaultZoneRawOffsetMs=" + def.getRawOffset()
                + " zoneAgrees=" + zoneAgrees
                + " defaultPattern=" + defaultPattern);
    }

    public static void main(String[] args) throws Exception {
        logManagerSingleton();
        getLoggerIdentityAndParents();
        levelFiltering();
        handlerChain();
        parentHandlerDelivery();
        supplierOverloads();
        recordPayloads();
        sourcePairStateMachine();
        publishedRecordCarriesItsCaller();
        formattedOutputIsRealBytes();
        handlerLevelIsASecondGate();
        defaultZoneReachesTheFormatter();
        System.out.println("CK RJdkLogging checks=" + checks);
        System.out.println("PASS RJdkLogging (" + checks + " checks)");
    }
}
