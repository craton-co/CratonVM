import java.util.Arrays;
import java.util.LinkedHashSet;
import java.util.Set;

import org.junit.runner.Description;
import org.junit.runner.JUnitCore;
import org.junit.runner.Request;
import org.junit.runner.Result;
import org.junit.runner.manipulation.Filter;

/**
 * Local-only harness: run a SUBSET of one class's {@code @Test} methods inside
 * a single JVM <em>and a single class fixture</em>.
 *
 * <p>Needed to reproduce ordering-dependent failures — a method that passes
 * alone but fails in a full-class run — without paying for the whole class.
 *
 * <p>The subset is expressed as a JUnit {@link Filter} on ONE {@link Request},
 * NOT as a sequence of {@code Request.method(...)} runs. That distinction is
 * the whole point: separate requests re-run {@code @BeforeClass}/
 * {@code @AfterClass} per method, so anything the class fixture shares between
 * methods (for Tomcat's {@code LoggingBaseTest}, the per-class temp directory —
 * i.e. the appBase path that a redeploy test keeps rewriting) is recreated and
 * the pollution being hunted never happens. Methods run in JUnit's own order,
 * which is what the full-class run does too.
 *
 * <p>usage: {@code RunMethods <class> <method1> [<method2> ...]}
 */
public final class RunMethods {
    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            throw new IllegalArgumentException("usage: <class> <method>...");
        }
        Class<?> testClass = Class.forName(args[0]);
        final Set<String> wanted = new LinkedHashSet<>(Arrays.asList(args).subList(1, args.length));

        Filter filter = new Filter() {
            @Override
            public boolean shouldRun(Description description) {
                if (description.isTest()) {
                    String m = description.getMethodName();
                    if (m == null) {
                        return false;
                    }
                    // Parameterized descriptions look like "testFoo[0: x]".
                    int bracket = m.indexOf('[');
                    return wanted.contains(bracket < 0 ? m : m.substring(0, bracket));
                }
                for (Description child : description.getChildren()) {
                    if (shouldRun(child)) {
                        return true;
                    }
                }
                return false;
            }

            @Override
            public String describe() {
                return "methods " + wanted;
            }
        };

        long t0 = System.currentTimeMillis();
        Result result = new JUnitCore().run(Request.aClass(testClass).filterWith(filter));
        long ms = System.currentTimeMillis() - t0;

        System.out.println("[RunMethods] ran=" + result.getRunCount()
                + " failed=" + result.getFailureCount() + " (" + ms + "ms)");
        for (var failure : result.getFailures()) {
            System.out.println("[RunMethods] FAILURE " + failure.getTestHeader() + ": "
                    + failure.getMessage());
            System.out.println(failure.getTrace());
        }
        System.out.println("[RunMethods] total failures = " + result.getFailureCount());
        if (!result.wasSuccessful()) {
            System.exit(1);
        }
    }
}
