import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherFactory;
import org.junit.platform.launcher.listeners.SummaryGeneratingListener;

/** Temporary selector for parameterless Hibernate JUnit methods. */
public final class AnyMethodRunner {
    public static void main(String[] args) throws Exception {
        if (args.length != 2) throw new IllegalArgumentException("usage: AnyMethodRunner <class> <method>");
        Class<?> type = Class.forName(args[0], false, AnyMethodRunner.class.getClassLoader());
        var listener = new SummaryGeneratingListener();
        var request = LauncherDiscoveryRequestBuilder.request()
                .selectors(DiscoverySelectors.selectMethod(type, args[1]))
                .build();
        long started = System.nanoTime();
        var launcher = LauncherFactory.create();
        launcher.registerTestExecutionListeners(listener);
        launcher.execute(request);
        var summary = listener.getSummary();
        long elapsedMs = (System.nanoTime() - started) / 1_000_000;
        System.out.println("@@RESULT found=" + summary.getTestsFoundCount()
                + " started=" + summary.getTestsStartedCount()
                + " ok=" + summary.getTestsSucceededCount()
                + " failed=" + summary.getTestsFailedCount()
                + " ms=" + elapsedMs);
        summary.getFailures().forEach(f -> f.getException().printStackTrace(System.out));
        if (summary.getTotalFailureCount() != 0) System.exit(1);
    }
}
