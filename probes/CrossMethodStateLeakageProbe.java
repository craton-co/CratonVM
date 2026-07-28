import java.util.HashSet;
import java.util.Properties;
import java.util.Set;

import ch.qos.logback.classic.LoggerContext;

/**
 * Real-JDK regression probe for the state-isolation contracts exercised by the
 * Spring Boot logging tests. It deliberately performs the same sequence that
 * JUnit performs within one test-class process: establish a baseline, mutate
 * process state, restore the baseline, then make a fresh assertion.
 */
public final class CrossMethodStateLeakageProbe {

    private static final String ROLLING_SIZE = "LOG4J2_ROLLINGPOLICY_MAX_FILE_SIZE";

    public static void main(String[] args) {
        propertiesKeySetRestoreIsLive();
        loggerContextResetClearsObjects();
        System.out.println("CROSSMETHOD_PROBE_OK");
    }

    private static void propertiesKeySetRestoreIsLive() {
        Properties properties = System.getProperties();
        properties.remove(ROLLING_SIZE);
        Set<Object> baseline = new HashSet<>(properties.keySet());

        properties.setProperty(ROLLING_SIZE, "52428800");
        require("52428800".equals(System.getProperty(ROLLING_SIZE)), "first property write was lost");

        properties.keySet().retainAll(baseline);
        require(System.getProperty(ROLLING_SIZE) == null, "keySet retainAll did not restore system properties");

        properties.setProperty(ROLLING_SIZE, "26214400");
        require("26214400".equals(System.getProperty(ROLLING_SIZE)), "stale sibling value survived cleanup");
        properties.remove(ROLLING_SIZE);
    }

    private static void loggerContextResetClearsObjects() {
        LoggerContext context = new LoggerContext();
        String key = "crossmethod.pattern.rules";
        context.putObject(key, "com.example.Alpha");
        require("com.example.Alpha".equals(context.getObject(key)), "logger context did not retain test setup");

        context.reset();
        require(context.getObject(key) == null, "LoggerContext.reset leaked a sibling test object");
    }

    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }
}
