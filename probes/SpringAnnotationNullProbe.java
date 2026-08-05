import java.lang.reflect.Method;
import org.springframework.core.annotation.AnnotatedElementUtils;
import org.springframework.core.annotation.MergedAnnotations;
import org.springframework.core.annotation.Order;

/**
 * Local reproducer for the JIT-only regression that makes Spring's
 * configuration-class parsing die with
 *
 * <pre>
 * NullPointerException: Cannot invoke "MergedAnnotations.isPresent(String)"
 *   because the return value of "AnnotatedElementUtils.getAnnotations(AnnotatedElement)" is null
 *     at AnnotatedElementUtils.isAnnotated(AnnotatedElementUtils.java:232)
 *     at StandardAnnotationMetadata.isAnnotatedMethod(StandardAnnotationMetadata.java:168)
 * </pre>
 *
 * <p>`getAnnotations` is `return MergedAnnotations.from(element, ..)` — a static
 * factory with no null return — so a null here is the VM's, not Spring's.
 *
 * <p>This needs only `spring-core`, not the Spring Boot test fixture, which is
 * what makes it runnable off the suite host. It drives the exact two frames the
 * suite stack names, hot enough to compile, and reports the first null it sees
 * along with the iteration, so a bisect step is one short run rather than a
 * 55-second 53-test class.
 *
 * <pre>
 * javac -cp spring-core.jar:jspecify.jar -d /tmp/probe probes/SpringAnnotationNullProbe.java
 * java     -cp /tmp/probe:... SpringAnnotationNullProbe            # control
 * cratonvm --java-home &lt;jdk&gt; -cp /tmp/probe:... SpringAnnotationNullProbe
 * </pre>
 */
public class SpringAnnotationNullProbe {

    /** Stand-in for a Spring `@Configuration` class: annotated methods, plain methods. */
    @Order(1)
    public static class Config {
        @Order(1)
        public String beanA() {
            return "a";
        }

        @SafeVarargs
        public final String beanB(String... s) {
            return "b";
        }

        public String plain() {
            return "c";
        }

        @Order(2)
        public String beanD() {
            return "d";
        }
    }

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        Method[] methods = Config.class.getDeclaredMethods();
        String ann = Order.class.getName();

        long nullsFromGetAnnotations = 0;
        long nullsFromFrom = 0;
        long isAnnotatedCalls = 0;
        long firstNullIter = -1;

        for (int i = 0; i < iters; i++) {
            for (Method m : methods) {
                // `AnnotatedElementUtils.getAnnotations` is private, so drive it
                // exactly the way the suite stack does — through `isAnnotated`,
                // which is the frame that threw (AnnotatedElementUtils.java:232).
                // A null from the private factory surfaces here as the NPE.
                try {
                    if (AnnotatedElementUtils.isAnnotated(m, ann)) {
                        isAnnotatedCalls++;
                    }
                } catch (NullPointerException npe) {
                    nullsFromGetAnnotations++;
                    if (firstNullIter < 0) {
                        firstNullIter = i;
                        System.out.println("  first NPE: " + npe.getMessage());
                    }
                    continue;
                }
                // The public factory on the same element, checked directly: it
                // has no null return either, so a null here localises the fault
                // without needing the private frame.
                MergedAnnotations direct =
                        MergedAnnotations.from(m, MergedAnnotations.SearchStrategy.DIRECT);
                if (direct == null) {
                    nullsFromFrom++;
                    if (firstNullIter < 0) {
                        firstNullIter = i;
                    }
                }
            }
            // Allocation pressure, so a young collection can land anywhere in
            // the loop rather than only at its edges.
            if ((i & 7) == 0) {
                byte[] churn = new byte[256];
                churn[0] = (byte) i;
            }
        }

        System.out.println("getAnnotations() returned null : " + nullsFromGetAnnotations);
        System.out.println("MergedAnnotations.from() null  : " + nullsFromFrom);
        System.out.println("isAnnotated() true count       : " + isAnnotatedCalls);
        System.out.println("first null at iteration        : " + firstNullIter);
        boolean ok = nullsFromGetAnnotations == 0 && nullsFromFrom == 0 && isAnnotatedCalls > 0;
        System.out.println(ok ? "SPRINGNULL PASS" : "SPRINGNULL FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
