import java.lang.reflect.Method;

import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.TestExecutionListener;
import org.junit.platform.launcher.TestIdentifier;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherFactory;

/** Fixture-local runner for one injected-parameter JUnit method. */
public final class MethodRunner {
	public static void main(String[] args) throws Exception {
		if (args.length != 2) throw new IllegalArgumentException("class method");
		System.out.printf("@@PROPS forks=%s iterations=%s parsed_forks=%d parsed_iterations=%d%n",
				System.getProperty("craton.smoke.forks"), System.getProperty("craton.smoke.iterations"),
				Integer.getInteger("craton.smoke.forks", 50), Integer.getInteger("craton.smoke.iterations", 400));
		Class<?> testClass = Class.forName(args[0], false, MethodRunner.class.getClassLoader());
		Method method = null;
		for (Method candidate : testClass.getDeclaredMethods()) {
			if (candidate.getName().equals(args[1])) { method = candidate; break; }
		}
		if (method == null) throw new NoSuchMethodException(args[0] + "#" + args[1]);
		LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
				.selectors(DiscoverySelectors.selectMethod(testClass, method)).build();
		Launcher launcher = LauncherFactory.create();
		long[] result = new long[3];
		launcher.registerTestExecutionListeners(new TestExecutionListener() {
			@Override public void executionStarted(TestIdentifier id) {
				if (id.isTest()) result[2] = System.nanoTime();
			}
			@Override public void executionFinished(TestIdentifier id, TestExecutionResult status) {
				if (id.isTest()) {
					result[0]++;
					if (status.getStatus() == TestExecutionResult.Status.SUCCESSFUL) result[1]++;
					else status.getThrowable().ifPresent(t -> t.printStackTrace(System.err));
				}
			}
		});
		long start = System.nanoTime();
		launcher.execute(request);
		long elapsed = (System.nanoTime() - start) / 1_000_000L;
		long testElapsed = result[2] == 0 ? -1 : (System.nanoTime() - result[2]) / 1_000_000L;
		System.out.printf("@@RESULT %s#%s found=%d started=%d ok=%d failed=%d ms=%d test_ms=%d%n",
				args[0], args[1], result[0], result[0], result[1], result[0] - result[1], elapsed, testElapsed);
		if (result[0] != 1 || result[1] != 1) System.exit(1);
	}
}
