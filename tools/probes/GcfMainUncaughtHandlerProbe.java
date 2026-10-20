/**
 * gcf f4/r (2026-10-01): an exception escaping {@code main} goes to the main
 * thread's uncaught handler, as on HotSpot
 * (gcf-f2r-main-thread-uncaught-exception-ignores-the-default-handler).
 *
 * <p>Modes (first argument), each ending with a shutdown hook that prints
 * {@code hook} and an uncaught {@code IllegalStateException("x")}; rc 1 on
 * every mode. HotSpot 25.0.3:
 * <ul>
 *   <li>{@code default}: stdout {@code handler main java.lang.IllegalStateException: x},
 *       {@code hook}; stderr empty;</li>
 *   <li>{@code thread} (per-thread handler on main): stdout
 *       {@code thread-handler main ...}, {@code hook}; stderr empty;</li>
 *   <li>{@code both}: the per-thread handler wins: {@code own ...}, {@code hook};</li>
 *   <li>{@code throws} (the default handler throws): stdout
 *       {@code handler-throws ...}, {@code hook}; stderr an empty line, then
 *       {@code Exception: java.lang.UnsupportedOperationException thrown from the
 *       UncaughtExceptionHandler in thread "main"};</li>
 *   <li>{@code renamed}: the default handler, with main renamed: {@code handler
 *       boss ...};</li>
 *   <li>{@code none} (positive control for the trace path): stdout {@code hook};
 *       stderr {@code Exception in thread "main" java.lang.IllegalStateException: x}
 *       and one {@code at} line.</li>
 * </ul>
 */
public class GcfMainUncaughtHandlerProbe {
    public static void main(String[] args) {
        String m = args.length > 0 ? args[0] : "default";
        switch (m) {
            case "thread" -> Thread.currentThread().setUncaughtExceptionHandler(
                    (t, e) -> System.out.println("thread-handler " + t.getName() + " " + e));
            case "throws" -> Thread.setDefaultUncaughtExceptionHandler((t, e) -> {
                System.out.println("handler-throws " + e);
                throw new UnsupportedOperationException("from handler");
            });
            case "both" -> {
                Thread.setDefaultUncaughtExceptionHandler((t, e) -> System.out.println("default " + e));
                Thread.currentThread().setUncaughtExceptionHandler((t, e) -> System.out.println("own " + e));
            }
            case "renamed" -> {
                Thread.currentThread().setName("boss");
                Thread.setDefaultUncaughtExceptionHandler(
                        (t, e) -> System.out.println("handler " + t.getName() + " " + e));
            }
            case "none" -> { }
            default -> Thread.setDefaultUncaughtExceptionHandler(
                    (t, e) -> System.out.println("handler " + t.getName() + " " + e));
        }
        Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("hook")));
        throw new IllegalStateException("x");
    }
}
