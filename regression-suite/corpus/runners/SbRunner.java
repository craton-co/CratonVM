import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.launcher.*;
import org.junit.platform.launcher.core.*;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Stand-in for the corpus' JUnit runner (the original {@code apps/spring-boot/sb-runner/SbRunner.java} is not
 * checked in): runs the named classes through the JUnit Platform launcher and prints ONE line,
 * {@code SBRUNNER_RESULT tests=N failed=F aborted=A skipped=S containersFailed=C}, which is what
 * {@code run-corpus.sh} compares. Every failed test is also printed, as {@code SBFAIL <unique id> :: <throwable>}
 * (a failed container as {@code SBCONTAINERFAIL}), so two arms with equal counts can still be told apart.
 */
public class SbRunner {
    public static void main(String[] args) throws Exception {
        LauncherDiscoveryRequestBuilder b = LauncherDiscoveryRequestBuilder.request();
        for (String a : args) b.selectors(DiscoverySelectors.selectClass(a));
        AtomicInteger tests = new AtomicInteger(), failed = new AtomicInteger(), aborted = new AtomicInteger(),
                skipped = new AtomicInteger(), cfailed = new AtomicInteger();
        Launcher l = LauncherFactory.create();
        l.execute(b.build(), new TestExecutionListener() {
            public void executionSkipped(TestIdentifier id, String r) {
                if (!id.isTest()) return;
                tests.incrementAndGet(); skipped.incrementAndGet();
            }
            public void executionFinished(TestIdentifier id, TestExecutionResult r) {
                String m = r.getThrowable().map(t -> String.valueOf(t).replace('\n', ' ')).orElse("?");
                if (id.isTest()) {
                    tests.incrementAndGet();
                    if (r.getStatus() == TestExecutionResult.Status.FAILED) {
                        failed.incrementAndGet();
                        System.err.println("SBFAIL " + id.getUniqueId() + " :: " + m);
                    } else if (r.getStatus() == TestExecutionResult.Status.ABORTED) aborted.incrementAndGet();
                } else if (r.getStatus() == TestExecutionResult.Status.FAILED) {
                    cfailed.incrementAndGet();
                    System.err.println("SBCONTAINERFAIL " + id.getUniqueId() + " :: " + m);
                }
            }
        });
        System.out.println("SBRUNNER_RESULT tests=" + tests + " failed=" + failed + " aborted=" + aborted
                + " skipped=" + skipped + " containersFailed=" + cfailed);
        System.exit(failed.get() + cfailed.get() == 0 ? 0 : 1);
    }
}
