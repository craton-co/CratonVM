import java.util.Properties;

public class SystemSetPropertiesResetRepro {
    public static void main(String[] args) {
        Properties baseline = (Properties) System.getProperties().clone();
        baseline.remove("kc.config.args");

        System.setProperty("kc.config.args", "--telemetry-service-name=something3");
        check("--telemetry-service-name=something3".equals(System.getProperty("kc.config.args")),
                "setProperty did not publish kc.config.args");

        System.setProperties((Properties) baseline.clone());
        check(System.getProperty("kc.config.args") == null,
                "System.setProperties left stale kc.config.args in System.getProperty");
        check(System.getProperties().getProperty("kc.config.args") == null,
                "System.setProperties left stale kc.config.args in System.getProperties().getProperty");
        check(!System.getProperties().stringPropertyNames().contains("kc.config.args"),
                "System.setProperties left stale kc.config.args in stringPropertyNames");

        Properties oldSystemProps = System.getProperties();
        oldSystemProps.setProperty("kc.config.args", "next");
        check("next".equals(System.getProperty("kc.config.args")),
                "current System.getProperties() no longer mirrors writes to System.getProperty");

        System.setProperties((Properties) baseline.clone());
        check(System.getProperties() != oldSystemProps,
                "System.setProperties did not swap the System.getProperties() singleton");

        oldSystemProps.setProperty("kc.config.args", "stale-old-reference");
        check(System.getProperty("kc.config.args") == null,
                "old System.getProperties() reference still mutates global system properties");

        System.setProperty("kc.config.args", "via-new-singleton");
        check("via-new-singleton".equals(System.getProperties().getProperty("kc.config.args")),
                "new System.getProperties() singleton did not observe static setProperty");
    }

    private static void check(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }
}
