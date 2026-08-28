import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClass;

import org.junit.platform.engine.FilterResult;
import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.engine.UniqueId;
import org.junit.platform.launcher.PostDiscoveryFilter;
import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.TestExecutionListener;
import org.junit.platform.launcher.TestIdentifier;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherFactory;

import java.util.concurrent.atomic.AtomicInteger;

/**
 * Run ONE test method — every parameterisation of it — and print a line per
 * invocation.
 *
 * `CratonRunner` runs a whole class and `PerTestProgressRunner` prints per test
 * within a class; neither can isolate a single method. That matters when the
 * method under investigation is most of what its class costs:
 * `OpenSslEngineTest.mustCallResumeTrustedOnSessionResumption` is 2 618 s of a
 * ~2.4 h class because each failing parameterisation burns a 60 s JUnit
 * timeout, so running the class to observe one method is not affordable and
 * running it repeatedly is not affordable at all.
 *
 * The per-invocation lines are what make a partial failure legible: this method
 * fails a SUBSET of its 48 parameterisations, and WHICH subset is the finding
 * (`known-issues/netty/java-reentry-from-boringssl-verify-callback-loses-the-tls13-client-cert-20260826.md`
 * decodes them against `OpenSslEngineTestParam.expandCombinations`).
 *
 * Output:
 *   @@INV <n> <PASS|FAIL> <displayName>
 *   @@METHOD <class>#<method> found=.. ok=.. failed=.. ms=..
 *
 * usage: MethodProgressRunner <fully.qualified.TestClass> <methodName>
 */
public final class MethodProgressRunner {
    private MethodProgressRunner() {}

    public static void main(String[] args) {
        if (args.length < 2) {
            System.err.println("usage: MethodProgressRunner <fully.qualified.TestClass> <methodName>");
            System.exit(2);
        }
        String className = args[0];
        String methodName = args[1];

        final AtomicInteger index = new AtomicInteger();
        final AtomicInteger ok = new AtomicInteger();
        final AtomicInteger failed = new AtomicInteger();

        // Select the CLASS and prune to the one method, rather than
        // `selectMethod(className, methodName)`.
        //
        // That selector answers `found=0` here, for two reasons at once:
        // `mustCallResumeTrustedOnSessionResumption` is declared on the base
        // `SSLEngineTest`, not on `OpenSslEngineTest`, and it takes an
        // `SSLEngineTestParam` — so the no-parameter-types overload matches
        // nothing and the discovery is silently empty. A post-discovery filter
        // over the class needs neither the declaring class nor the parameter
        // spelling, which is what makes this runner usable on an inherited
        // `@ParameterizedTest` without hand-writing a signature per method.
        LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
                .selectors(selectClass(className))
                .filters((PostDiscoveryFilter) descriptor -> {
                    UniqueId.Segment last = descriptor.getUniqueId().getLastSegment();
                    String type = last.getType();
                    // Only method-level segments are judged. Engines, classes
                    // and per-invocation children are kept: an invocation whose
                    // template was excluded is pruned with it.
                    if ("method".equals(type) || "test-template".equals(type)) {
                        return last.getValue().startsWith(methodName + "(")
                                ? FilterResult.included("selected method")
                                : FilterResult.excluded("other method");
                    }
                    return FilterResult.included("not a method segment");
                })
                .build();
        Launcher launcher = LauncherFactory.create();
        launcher.registerTestExecutionListeners(new TestExecutionListener() {
            @Override
            public void executionFinished(TestIdentifier id, TestExecutionResult status) {
                if (!id.isTest()) {
                    return;
                }
                boolean pass = status.getStatus() == TestExecutionResult.Status.SUCCESSFUL;
                if (pass) {
                    ok.incrementAndGet();
                } else {
                    failed.incrementAndGet();
                }
                // The invocation index is what the page's failing-index sets are
                // written in, so it is printed even for a pass.
                System.out.println("@@INV " + index.incrementAndGet()
                        + " " + (pass ? "PASS" : "FAIL")
                        + " " + id.getDisplayName());
                if (!pass) {
                    status.getThrowable().ifPresent(t ->
                            System.out.println("@@INVCAUSE " + index.get() + " "
                                    + t.getClass().getName() + ": " + t.getMessage()));
                }
                System.out.flush();
            }
        });

        long t0 = System.nanoTime();
        launcher.execute(request);
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        System.out.printf("@@METHOD %s#%s found=%d ok=%d failed=%d ms=%d%n",
                className, methodName, index.get(), ok.get(), failed.get(), ms);
        System.out.flush();
        if (failed.get() != 0) {
            System.exit(1);
        }
    }
}
