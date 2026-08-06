import java.lang.reflect.Constructor;
import java.lang.reflect.Method;

/**
 * Which routes to a real {@code java.lang.System.Logger} actually work under
 * {@code --jdk-only}?
 *
 * Written because the block comment above {@code register_system_logger_methods}
 * in {@code native-builtins/src/lib.rs} answered that question by READING the
 * JDK sources -- "not fixed by deleting the shadow and letting the real JDK
 * chain run", because {@code Reflection.getCallerClass()} returns null on
 * CratonVM boot frames and the {@code LoggerFinderLoader}/{@code ServiceLoader}/
 * {@code BootstrapLogger} chain has never been exercised. That reasoning is
 * right about {@code System.getLogger} and wrong about the whole surface: one
 * of the four routes below constructs a real logger under {@code --jdk-only}
 * today, which is what let the refused {@code cratonvm/internal/SystemLogger}
 * fabrication land on something real instead of on a
 * {@code NoClassDefFoundError}.
 *
 * Each route prints its result class, or the throwable, and never both -- the
 * point of the probe is that a route that throws must not read as absent
 * output. The last line is a tally sentinel, for the same reason every other
 * strict probe has one: a killed run prints a truncated transcript that reads
 * exactly like a clean short one.
 *
 * Run it with the internal packages opened, or the reflective routes report an
 * access failure rather than the answer they exist to give:
 *
 * <pre>
 * cratonvm --jdk-only --java-home $JDK25 \
 *     --add-opens java.base/jdk.internal.logger=ALL-UNNAMED \
 *     --add-opens java.base/sun.util.logging=ALL-UNNAMED \
 *     -cp probes LoggerRoutes
 * </pre>
 *
 * Measured 2026-08-06, Azure Linux, Temurin 25.0.3, against a HotSpot control:
 *
 * <pre>
 *                            HotSpot 25                  cratonvm --jdk-only
 * System.getLogger           JULWrapper                  NoClassDefFoundError
 * SimpleConsoleLogger ctor   SimpleConsoleLogger         SimpleConsoleLogger
 * LazyLoggers.getLogger      JULWrapper                  NoClassDefFoundError
 * PlatformLogger             PlatformLogger              PlatformLogger
 * </pre>
 *
 * The two that diverge are the two CratonVM registers a native for; the two
 * that agree are the two it does not. That is the whole finding.
 */
public class LoggerRoutes {

    interface Route {
        Object run() throws Throwable;
    }

    static void route(String label, Route r) {
        try {
            System.out.println(label + " => " + r.run());
        } catch (Throwable t) {
            // Unwrap the reflective wrapper: `InvocationTargetException: X` is
            // one level of noise in front of the only interesting name.
            Throwable cause = (t instanceof java.lang.reflect.InvocationTargetException && t.getCause() != null)
                    ? t.getCause() : t;
            System.out.println(label + " !! " + cause);
        }
    }

    /** Everything a caller of `System.getLogger` actually uses, in one line. */
    static String describe(Object logger) {
        System.Logger l = (System.Logger) logger;
        return logger.getClass().getName()
                + " name=" + l.getName()
                + " info=" + l.isLoggable(System.Logger.Level.INFO)
                + " debug=" + l.isLoggable(System.Logger.Level.DEBUG);
    }

    public static void main(String[] args) {
        route("System.getLogger", () -> describe(System.getLogger("probe")));

        // The route the strict-mode fallback takes. Package-private
        // constructor, so this is `setAccessible`, not ordinary API -- from
        // inside the VM the native calls it directly.
        route("SimpleConsoleLogger ctor", () -> {
            Class<?> c = Class.forName("jdk.internal.logger.SimpleConsoleLogger");
            Constructor<?> k = c.getDeclaredConstructor(String.class, boolean.class);
            k.setAccessible(true);
            return describe(k.newInstance("probe", false));
        });

        // The boot-path entry `System.getLogger` delegates to once it has a
        // caller module. CratonVM registers a native for this one too.
        route("LazyLoggers.getLogger", () -> {
            Class<?> c = Class.forName("jdk.internal.logger.LazyLoggers");
            Method m = c.getDeclaredMethod("getLogger", String.class, Module.class);
            m.setAccessible(true);
            return describe(m.invoke(null, "probe", Object.class.getModule()));
        });

        // Not a `System.Logger` at all -- the platform-logging facade. Included
        // because it shares the `DefaultLoggerFinder` machinery and therefore
        // separates "the finder chain is broken" from "these two triples have
        // a native".
        route("PlatformLogger", () -> {
            Class<?> c = Class.forName("sun.util.logging.PlatformLogger");
            Method m = c.getDeclaredMethod("getLogger", String.class);
            m.setAccessible(true);
            return m.invoke(null, "probe").getClass().getName();
        });

        System.out.println("LOGGERROUTES-DONE");
    }
}
