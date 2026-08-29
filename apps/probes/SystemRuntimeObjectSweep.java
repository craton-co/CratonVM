import java.io.InputStream;
import java.io.PrintStream;
import java.lang.System.Logger;
import java.lang.System.Logger.Level;
import java.util.Properties;

/** L8 tail — `java.lang.System` (23 rows), `Runtime` (8), `Object` (6) and
 *  `System$Logger` (6): 43 of the unowned `--jdk-only` surface.
 *
 *  These are the batch where "ask the question" is harder than the answer,
 *  because half the surface is irreversible and the other half is not
 *  comparable across two VMs at all:
 *
 *  * **`System.exit` and `Runtime.exit` are never called.** A probe that exits
 *    is a probe with no tail, and the row it would buy is one the harness's own
 *    exit-code handling already covers.
 *  * **Values that legitimately differ are not asked.** `java.vm.name`,
 *    `java.vm.vendor` and the identity hash inside `Object.toString()` are
 *    *supposed* to differ between HotSpot and CratonVM; asking them would be
 *    36 rows of noise around the four that matter. What IS asked is the SHAPE:
 *    that `toString()` starts with the class name and an `@`, that it is stable
 *    across calls, and that two distinct objects render differently.
 *  * **Mutators are restored.** `setOut`/`setErr`/`setIn` and every property
 *    write put back what they found, and the row after each one checks that
 *    the restore took — because a probe that corrupts `System.out` halfway
 *    through produces a truncated file, and a truncated file reads as a clean
 *    diff for every row it never printed.
 *
 *  DETERMINISM: no timing, no identity hash codes, no environment enumeration
 *  (the two VMs may inject different variables; specific keys are asked
 *  instead), no wall clock.
 */
public class SystemRuntimeObjectSweep {

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static final char[] HEX = "0123456789abcdef".toCharArray();
    /** Captured before anything can redirect it. */
    static final PrintStream OUT = System.out;

    static String esc(String s) {
        if (s == null) {
            return "null";
        }
        char[] out = new char[s.length() * 6];
        int n = 0;
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) {
                out[n++] = '\\';
                out[n++] = 'u';
                out[n++] = HEX[(c >> 12) & 0xf];
                out[n++] = HEX[(c >> 8) & 0xf];
                out[n++] = HEX[(c >> 4) & 0xf];
                out[n++] = HEX[c & 0xf];
            } else {
                out[n++] = c;
            }
        }
        return new String(out, 0, n);
    }

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            v = String.valueOf(f.get());
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName() + ": " + e.getMessage();
        }
        OUT.println(esc(tag) + " |" + esc(v) + "|");
    }

    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            OUT.println("SECTION-ABORTED " + name + " " + e.getClass().getName());
        }
    }

    // ------------------------------------------------------ 1. the properties

    /** Keys whose value is fixed by the JDK image or by the process, so both
     *  VMs must agree. `java.vm.*` is deliberately absent: those name the VM
     *  and are SUPPOSED to differ. */
    static final String[] SPEC_KEYS = {
        "java.version", "java.specification.version", "java.specification.name",
        "java.class.version", "file.separator", "path.separator", "line.separator",
        "os.arch", "os.name", "user.dir", "java.io.tmpdir", "file.encoding",
        "native.encoding", "stdout.encoding", "sun.jnu.encoding",
    };

    static void properties() {
        for (String k : SPEC_KEYS) {
            p("[getProperty " + k + "]", () -> System.getProperty(k));
        }
        p("getProperty missing", () -> System.getProperty("no.such.property.at.all"));
        p("getProperty missing with default",
            () -> System.getProperty("no.such.property.at.all", "fallback"));
        p("getProperty present with default",
            () -> System.getProperty("file.separator", "fallback"));
        p("getProperty empty name", () -> System.getProperty(""));
        p("getProperty null", () -> System.getProperty(null));
        p("getProperty null with default", () -> System.getProperty(null, "d"));
        p("lineSeparator", () -> System.lineSeparator());
        p("lineSeparator == line.separator",
            () -> System.lineSeparator().equals(System.getProperty("line.separator")));
        p("lineSeparator is stable", () -> System.lineSeparator() == System.lineSeparator());

        // The mutators, each followed by the row that says the restore took.
        p("setProperty returns the old value", () -> {
            String old = System.setProperty("cratonvm.probe.key", "one");
            return String.valueOf(old);
        });
        p("setProperty then getProperty", () -> System.getProperty("cratonvm.probe.key"));
        p("setProperty again returns the previous",
            () -> System.setProperty("cratonvm.probe.key", "two"));
        p("clearProperty returns the value", () -> System.clearProperty("cratonvm.probe.key"));
        p("clearProperty is now gone", () -> System.getProperty("cratonvm.probe.key"));
        p("clearProperty of an absent key", () -> System.clearProperty("cratonvm.probe.key"));
        p("setProperty null name", () -> System.setProperty(null, "v"));
        p("setProperty null value", () -> System.setProperty("cratonvm.probe.key2", null));
        p("setProperty empty name", () -> System.setProperty("", "v"));
        p("clearProperty null", () -> System.clearProperty(null));
        p("clearProperty empty", () -> System.clearProperty(""));

        p("getProperties is a Properties", () -> System.getProperties().getClass().getName());
        p("getProperties is the same object each call",
            () -> System.getProperties() == System.getProperties());
        p("getProperties sees a setProperty", () -> {
            System.setProperty("cratonvm.probe.key3", "three");
            String v = System.getProperties().getProperty("cratonvm.probe.key3");
            System.clearProperty("cratonvm.probe.key3");
            return v;
        });
        p("a write through getProperties is visible to getProperty", () -> {
            System.getProperties().setProperty("cratonvm.probe.key4", "four");
            String v = System.getProperty("cratonvm.probe.key4");
            System.clearProperty("cratonvm.probe.key4");
            return v;
        });
        // setProperties(p) then restore. Done as ONE row so a failure cannot
        // leave the VM without its properties for the rest of the run.
        p("setProperties round trip", () -> {
            Properties original = System.getProperties();
            Properties replacement = new Properties();
            replacement.setProperty("cratonvm.probe.only", "yes");
            System.setProperties(replacement);
            String seen = System.getProperty("cratonvm.probe.only");
            String gone = System.getProperty("file.separator");
            System.setProperties(original);
            String back = System.getProperty("file.separator");
            return seen + "/" + gone + "/" + (back != null);
        });
        p("properties survived the round trip", () -> System.getProperty("java.version") != null);
    }

    // ----------------------------------------------------- 2. the environment

    static void environment() {
        p("getenv PATH is present", () -> System.getenv("PATH") != null);
        p("getenv missing", () -> System.getenv("NO_SUCH_ENV_VAR_AT_ALL"));
        p("getenv null", () -> System.getenv(null));
        p("getenv map is unmodifiable", () -> {
            try {
                System.getenv().put("x", "y");
                return "no throw";
            } catch (UnsupportedOperationException e) {
                return e.getClass().getName();
            }
        });
        p("getenv map contains PATH", () -> System.getenv().containsKey("PATH"));
        p("getenv map get agrees with getenv",
            () -> {
                String a = System.getenv("PATH");
                String b = System.getenv().get("PATH");
                return a == null ? b == null : a.equals(b);
            });
        p("getenv map is the same object", () -> System.getenv() == System.getenv());
    }

    // --------------------------------------------------------- 3. the streams

    static void streams() {
        p("System.out is non-null", () -> System.out != null);
        p("System.err is non-null", () -> System.err != null);
        p("System.in is non-null", () -> System.in != null);
        p("setOut round trip", () -> {
            PrintStream old = System.out;
            PrintStream replacement = new PrintStream(new java.io.ByteArrayOutputStream());
            System.setOut(replacement);
            boolean took = System.out == replacement;
            System.setOut(old);
            return took + "/" + (System.out == old);
        });
        p("setErr round trip", () -> {
            PrintStream old = System.err;
            PrintStream replacement = new PrintStream(new java.io.ByteArrayOutputStream());
            System.setErr(replacement);
            boolean took = System.err == replacement;
            System.setErr(old);
            return took + "/" + (System.err == old);
        });
        p("setIn round trip", () -> {
            InputStream old = System.in;
            InputStream replacement = new java.io.ByteArrayInputStream(new byte[0]);
            System.setIn(replacement);
            boolean took = System.in == replacement;
            System.setIn(old);
            return took + "/" + (System.in == old);
        });
        p("setOut null then restore", () -> {
            PrintStream old = System.out;
            String r;
            try {
                System.setOut(null);
                r = "no throw, System.out==null is " + (System.out == null);
            } catch (Throwable e) {
                r = "THREW " + e.getClass().getName();
            }
            System.setOut(old);
            return r;
        });
        // `console()` is null for a redirected stdout, which is how both VMs
        // are run here. Asked as a nullness rather than as an object.
        p("console is null when redirected", () -> System.console() == null);
    }

    // ---------------------------------------------------------- 4. the logger

    static void loggers() {
        // WHICH PROVIDER, asked before anything it decides.
        //
        // `System.getLogger` returns whatever `jdk.internal.logger
        // .LoggerFinderLoader` selects, and the JDK ships two candidates whose
        // behaviour differs on rows below this one: `LoggingProviderImpl
        // $JULWrapper` (from `java.logging`, chosen whenever that module's
        // service provider is visible) and `SimpleConsoleLogger` (the fallback
        // used when no `LoggerFinder` service is found). They disagree about
        // `isLoggable(OFF)` and about which method a null `Level` is
        // dereferenced through, so this row is the CAUSE of the two below it
        // rather than a third symptom.
        //
        // Asked as a boolean rather than as a class name because CratonVM
        // legitimately substitutes its own logger in compatible mode, and the
        // question that matters in every mode is which SEMANTICS are in force.
        p("getLogger provider is the JUL wrapper",
            () -> System.getLogger("cratonvm.probe").getClass().getName().contains("JULWrapper"));
        p("getLogger name", () -> System.getLogger("cratonvm.probe").getName());
        p("getLogger class is a Logger",
            () -> System.getLogger("cratonvm.probe") instanceof Logger);
        p("getLogger twice is the same logger",
            () -> System.getLogger("cratonvm.probe") == System.getLogger("cratonvm.probe"));
        p("getLogger null name", () -> System.getLogger(null).getName());
        p("getLogger empty name", () -> System.getLogger("").getName());
        for (Level lv : Level.values()) {
            p("isLoggable " + lv, () -> System.getLogger("cratonvm.probe").isLoggable(lv));
        }
        p("Level values", () -> {
            String out = "";
            for (Level lv : Level.values()) {
                out = out + lv.name() + ":" + lv.getSeverity() + ",";
            }
            return out;
        });
        p("log at a disabled level does not throw", () -> {
            System.getLogger("cratonvm.probe").log(Level.TRACE, "probe-trace-line");
            return "no throw";
        });
        p("log with a null level", () -> {
            System.getLogger("cratonvm.probe").log((Level) null, "x");
            return "no throw";
        });
        p("getLogger with a bundle name",
            () -> System.getLogger("cratonvm.probe.bundle", null).getName());
    }

    // --------------------------------------------------------- 5. the runtime

    static void runtime() {
        p("getRuntime is non-null", () -> Runtime.getRuntime() != null);
        p("getRuntime is a singleton", () -> Runtime.getRuntime() == Runtime.getRuntime());
        p("getRuntime class", () -> Runtime.getRuntime().getClass().getName());
        p("version feature", () -> Runtime.version().feature());
        p("version interim", () -> Runtime.version().interim());
        p("version update", () -> Runtime.version().update());
        p("version patch", () -> Runtime.version().patch());
        p("version toString", () -> Runtime.version().toString());
        p("version build", () -> String.valueOf(Runtime.version().build()));
        p("version pre", () -> String.valueOf(Runtime.version().pre()));
        p("version equals itself", () -> Runtime.version().equals(Runtime.version()));
        p("version matches java.version",
            () -> Runtime.version().toString().startsWith(
                System.getProperty("java.specification.version")));
        p("availableProcessors is positive", () -> Runtime.getRuntime().availableProcessors() > 0);
        p("maxMemory is positive", () -> Runtime.getRuntime().maxMemory() > 0);

        // Shutdown hooks: added and removed in the same row, so the probe
        // leaves none behind for the harness to run.
        p("addShutdownHook then remove", () -> {
            Thread h = new Thread(() -> { });
            Runtime.getRuntime().addShutdownHook(h);
            boolean removed = Runtime.getRuntime().removeShutdownHook(h);
            return removed;
        });
        p("addShutdownHook twice", () -> {
            Thread h = new Thread(() -> { });
            Runtime.getRuntime().addShutdownHook(h);
            String r;
            try {
                Runtime.getRuntime().addShutdownHook(h);
                r = "no throw";
            } catch (Throwable e) {
                r = "THREW " + e.getClass().getName() + ": " + e.getMessage();
            }
            Runtime.getRuntime().removeShutdownHook(h);
            return r;
        });
        p("addShutdownHook of a started thread", () -> {
            Thread h = new Thread(() -> { });
            h.start();
            h.join();
            try {
                Runtime.getRuntime().addShutdownHook(h);
                Runtime.getRuntime().removeShutdownHook(h);
                return "no throw";
            } catch (Throwable e) {
                return "THREW " + e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("addShutdownHook null", () -> {
            Runtime.getRuntime().addShutdownHook(null);
            return "no throw";
        });
        p("removeShutdownHook of an unregistered thread",
            () -> Runtime.getRuntime().removeShutdownHook(new Thread(() -> { })));
        p("removeShutdownHook null", () -> Runtime.getRuntime().removeShutdownHook(null));

        p("gc does not throw", () -> {
            System.gc();
            return "no throw";
        });
        p("Runtime.gc does not throw", () -> {
            Runtime.getRuntime().gc();
            return "no throw";
        });
        p("runFinalization does not throw", () -> {
            System.runFinalization();
            return "no throw";
        });
        p("Runtime.runFinalization does not throw", () -> {
            Runtime.getRuntime().runFinalization();
            return "no throw";
        });
        // `load`/`loadLibrary` refusals. The MESSAGE of an UnsatisfiedLinkError
        // names a path, so only the type and the leading text are compared.
        p("System.load of a missing file", () -> {
            try {
                System.load("/no/such/library/at/all.so");
                return "no throw";
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("System.load of a relative path", () -> {
            try {
                System.load("relative.so");
                return "no throw";
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("System.load null", () -> {
            try {
                System.load(null);
                return "no throw";
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("System.loadLibrary of a missing library", () -> {
            try {
                System.loadLibrary("nosuchlibraryatall");
                return "no throw";
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("System.loadLibrary null", () -> {
            try {
                System.loadLibrary(null);
                return "no throw";
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("System.loadLibrary with a separator", () -> {
            try {
                System.loadLibrary("a/b");
                return "no throw";
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
    }

    // ------------------------------------------------- 6. the security manager

    static void security() {
        p("getSecurityManager", () -> String.valueOf(System.getSecurityManager()));
        p("setSecurityManager null", () -> {
            try {
                System.setSecurityManager(null);
                return "no throw";
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
    }

    // ----------------------------------------------------------- 7. Object

    static void objects() {
        Object a = new Object();
        Object b = new Object();
        p("new Object is non-null", () -> a != null);
        p("equals self", () -> a.equals(a));
        p("equals other", () -> a.equals(b));
        p("equals null", () -> a.equals(null));
        p("equals is reflexive on a String", () -> "x".equals("x"));
        // The identity hash inside toString() is not comparable; its SHAPE is.
        p("toString shape", () -> a.toString().startsWith("java.lang.Object@"));
        p("toString is stable", () -> a.toString().equals(a.toString()));
        p("toString differs between objects", () -> !a.toString().equals(b.toString()));
        p("toString tail is hex", () -> {
            String s = a.toString();
            String tail = s.substring(s.indexOf('@') + 1);
            for (int i = 0; i < tail.length(); i++) {
                if (Character.digit(tail.charAt(i), 16) < 0) {
                    return false;
                }
            }
            return tail.length() > 0;
        });
        p("toString agrees with hashCode", () -> {
            String s = a.toString();
            return s.equals("java.lang.Object@" + Integer.toHexString(a.hashCode()));
        });
        p("getClass", () -> a.getClass().getName());
        p("wait without a monitor", () -> {
            a.wait();
            return "no throw";
        });
        p("wait(1) without a monitor", () -> {
            a.wait(1);
            return "no throw";
        });
        p("wait(-1) with a monitor", () -> {
            synchronized (a) {
                a.wait(-1);
            }
            return "no throw";
        });
        p("wait(0,-1) with a monitor", () -> {
            synchronized (a) {
                a.wait(0, -1);
            }
            return "no throw";
        });
        p("wait(0,1000001) with a monitor", () -> {
            synchronized (a) {
                a.wait(0, 1000001);
            }
            return "no throw";
        });
        p("notify without a monitor", () -> {
            a.notify();
            return "no throw";
        });
        p("notifyAll without a monitor", () -> {
            a.notifyAll();
            return "no throw";
        });
        // `finalize` is protected; a subclass reaching super.finalize() is the
        // only call site a probe has, and in JDK 25 it must simply not throw.
        p("super.finalize", () -> {
            new Object() {
                String call() throws Throwable {
                    super.finalize();
                    return "no throw";
                }
            }.call();
            return "no throw";
        });
        p("clone of a plain Object", () -> {
            try {
                return new Object() {
                    Object call() throws Throwable {
                        return super.clone();
                    }
                }.call().getClass().getName();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
    }

    public static void main(String[] args) {
        sect("properties", SystemRuntimeObjectSweep::properties);
        sect("environment", SystemRuntimeObjectSweep::environment);
        sect("streams", SystemRuntimeObjectSweep::streams);
        sect("loggers", SystemRuntimeObjectSweep::loggers);
        sect("runtime", SystemRuntimeObjectSweep::runtime);
        sect("security", SystemRuntimeObjectSweep::security);
        sect("objects", SystemRuntimeObjectSweep::objects);
        OUT.println("rows " + rows);
        OUT.println("DONE SystemRuntimeObjectSweep");
    }
}
