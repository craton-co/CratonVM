import org.apache.commons.logging.Log;
import org.apache.commons.logging.LogFactory;

// SPB.9 (Session 114) repro: the ban's own note describes a real-world
// crash where per-class logger wiring during Spring Boot's component
// scan JIT-miscompiles the allocate-then-putfield sequence storing a
// freshly-obtained slf4j Logger into a Slf4jLog wrapper's field, leaving
// an int(1) where the Logger reference belongs ("expected object
// reference, got int(1)"). The three coupled facades are org/slf4j/
// (API), ch/qos/logback/ (backend), org/apache/commons/logging/ (the
// bridge Spring uses internally). This probe drives the exact same real
// bridge -- jcl-over-slf4j's LogFactory.getLog(Class), which internally
// wraps a real slf4j Logger backed by logback -- for many distinct
// classes (simulating per-class logger wiring across many components),
// then immediately uses each returned Log reference to log a real
// message, which would fail/crash if the reference were corrupted the
// way the ban describes.
public class SlfLoggingProbe {

    // Distinct nested classes so LogFactory.getLog(Class) creates
    // distinct per-class loggers each time, matching "per-class logger
    // wiring during component scan" rather than reusing one cached logger.
    static class C0 {} static class C1 {} static class C2 {} static class C3 {}
    static class C4 {} static class C5 {} static class C6 {} static class C7 {}
    static class C8 {} static class C9 {}

    private static final Class<?>[] CLASSES = {
        C0.class, C1.class, C2.class, C3.class, C4.class,
        C5.class, C6.class, C7.class, C8.class, C9.class,
    };

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        for (int i = 0; i < iterations; i++) {
            Class<?> target = CLASSES[i % CLASSES.length];
            Log log = LogFactory.getLog(target);
            if (log == null) {
                System.out.println("RESULT: FAIL at iteration " + i + " -- getLog returned null for " + target);
                System.exit(1);
            }
            // Use the reference for real -- this is exactly the
            // operation that would crash/misbehave if the stored
            // reference were corrupted to an int as the ban describes.
            log.info("probe message " + i + " from " + target.getSimpleName());
            if (!log.isInfoEnabled()) {
                System.out.println("RESULT: FAIL at iteration " + i + " -- isInfoEnabled() false, logger not wired correctly for " + target);
                System.exit(1);
            }
        }
        System.out.println("RESULT: OK -- " + iterations + " LogFactory.getLog(Class) + real log calls, all consistent");
    }
}
