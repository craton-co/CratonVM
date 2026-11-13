package cratonvm.wildfly;

import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

import org.jboss.logging.Logger;
import org.jboss.msc.service.ServiceName;
import org.wildfly.security.manager.WildFlySecurityManager;
import org.wildfly.security.manager.action.ReadPropertyAction;

/**
 * SPB.8c regression witness for the three formerly blanket-banned packages.
 *
 * This uses the real WildFly 32.0.1.Final module jars. The reflective action
 * preserves the exact GetAccessibleDeclaredFieldAction -> setAccessible path
 * named in the original crash report, while the other two loops directly drive
 * ServiceName.equals and Logger.getLogger/JDKLogger construction.
 */
public final class Spb8cWildflyMscLoggingProbe {
    private Spb8cWildflyMscLoggingProbe() {}

    private static void require(boolean condition, String what) {
        if (!condition) {
            throw new AssertionError(what);
        }
    }

    public static void main(String[] args) throws Exception {
        final int iterations = args.length == 0 ? 20_000 : Integer.parseInt(args[0]);
        final Class<?> actionClass = Class.forName(
                "org.wildfly.security.manager.GetAccessibleDeclaredFieldAction");
        final Constructor<?> actionCtor = actionClass.getDeclaredConstructor(
                Class.class, String.class);
        actionCtor.setAccessible(true);
        final Method run = actionClass.getDeclaredMethod("run");
        run.setAccessible(true);

        int assertions = 0;
        for (int i = 0; i < iterations; i++) {
            // This is the property-read/Long.parseLong chain named in SPB.8c.
            String number = new ReadPropertyAction(
                    "cratonvm.spb8c.number", Integer.toString(1_700_000_000 + (i & 7))).run();
            long parsed = Long.parseLong(number);
            require(parsed >= 1_700_000_000L && parsed <= 1_700_000_007L, "property parse");
            assertions++;

            // Keep the original package-private action and AccessibleObject path hot.
            Field out = (Field) run.invoke(actionCtor.newInstance(System.class, "out"));
            require(out != null && out.getDeclaringClass() == System.class, "accessible field action");
            assertions++;

            // Constructor and caller-stack setup from the original WildFly trace.
            WildFlySecurityManager manager = new WildFlySecurityManager();
            require(manager != null, "WildFlySecurityManager construction");
            assertions++;

            ServiceName left = ServiceName.of("craton", "spb8c", "service", Integer.toString(i & 31));
            ServiceName right = ServiceName.parse(left.getCanonicalName());
            require(left.equals(right), "ServiceName.equals");
            require(left.hashCode() == right.hashCode(), "ServiceName.hashCode");
            require(left.length() == 4, "ServiceName.length");
            assertions += 3;

            Logger logger = Logger.getLogger("cratonvm.spb8c." + (i & 7));
            require(logger != null, "LoggerProvider.getLogger");
            require(logger.getName().startsWith("cratonvm.spb8c."), "JDKLogger construction");
            assertions += 2;
        }

        System.out.println("SPB8C_PROBE_RESULT classes=3 iterations=" + iterations
                + " assertions=" + assertions + " failed=0");
    }
}
