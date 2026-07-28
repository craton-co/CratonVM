import java.nio.file.*;
import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.engine.discovery.DiscoverySelectors;
import org.junit.platform.launcher.*;
import org.junit.platform.launcher.core.*;

/**
 * Same contract as {@link CratonRunner}, plus a per-test-method wall-clock line:
 *
 *   @@METHOD <fqcn> <displayName> ms=<n> <SUCCESSFUL|FAILED|ABORTED>
 *
 * Used to attribute a class-level timeout to a specific method / fixture rather
 * than guessing from the class total.
 */
public class CratonRunnerTimed {
	public static void main(String[] args) throws Exception {
		String listFile = args[0];
		int start = Integer.parseInt( args[1] );
		List<String> classes = Files.readAllLines( Paths.get( listFile ) );
		int batch = Integer.getInteger( "craton.batch", Integer.MAX_VALUE );
		int ran = 0;

		for ( int i = start; i < classes.size(); i++ ) {
			if ( ran >= batch ) {
				System.out.println( "@@BATCHEND " + i );
				System.out.flush();
				System.exit( 0 );
			}
			ran++;
			String fqcn = classes.get( i ).trim();
			if ( fqcn.isEmpty() ) {
				continue;
			}
			System.out.println( "@@BEGIN " + i + " " + fqcn );
			System.out.flush();
			long t0 = System.nanoTime();
			try {
				// "pkg.Class" runs the whole class; "pkg.Class#method" runs one
				// method (still with its @BeforeEach/@AfterEach fixture), which
				// keeps throughput A/B runs to a sixth of the wall clock.
				final int hash = fqcn.indexOf( '#' );
				final String className = hash < 0 ? fqcn : fqcn.substring( 0, hash );
				Class<?> c = Class.forName( className, false, CratonRunnerTimed.class.getClassLoader() );
				Launcher launcher = LauncherFactory.create();
				TimingListener timing = new TimingListener( fqcn );
				// selectMethod(Class, String) only resolves no-arg methods, and
				// Hibernate's tests take an injected SessionFactoryScope, so look
				// the method up reflectively (walking superclasses -- these test
				// classes inherit every @Test from an abstract base).
				org.junit.platform.engine.DiscoverySelector selector;
				if ( hash < 0 ) {
					selector = DiscoverySelectors.selectClass( c );
				}
				else {
					final String methodName = fqcn.substring( hash + 1 );
					java.lang.reflect.Method found = null;
					for ( Class<?> k = c; k != null && found == null; k = k.getSuperclass() ) {
						for ( java.lang.reflect.Method m : k.getDeclaredMethods() ) {
							if ( m.getName().equals( methodName ) ) {
								found = m;
								break;
							}
						}
					}
					if ( found == null ) {
						throw new NoSuchMethodException( fqcn );
					}
					selector = DiscoverySelectors.selectMethod( c, found );
				}
				LauncherDiscoveryRequest req = LauncherDiscoveryRequestBuilder.request()
						.selectors( selector )
						.build();
				launcher.registerTestExecutionListeners( timing );
				launcher.execute( req );
				long ms = ( System.nanoTime() - t0 ) / 1_000_000;
				System.out.println( "@@RESULT " + i + " " + fqcn
						+ " found=" + timing.finished + " started=" + timing.finished
						+ " ok=" + timing.ok + " failed=" + timing.failed
						+ " aborted=" + timing.aborted + " skipped=0 ms=" + ms );
			}
			catch (Throwable t) {
				long ms = ( System.nanoTime() - t0 ) / 1_000_000;
				System.out.println( "@@RESULT " + i + " " + fqcn
						+ " found=0 started=0 ok=0 failed=0 aborted=0 skipped=0 ms=" + ms
						+ " loaderror=" + t.getClass().getName() );
			}
			System.out.flush();
		}
		System.out.println( "@@DONE" );
		System.out.flush();
	}

	static class TimingListener implements TestExecutionListener {
		final String fqcn;
		final Map<String, Long> starts = new ConcurrentHashMap<>();
		int ok, failed, aborted, finished;

		TimingListener(String fqcn) {
			this.fqcn = fqcn;
		}

		@Override
		public void executionStarted(TestIdentifier id) {
			starts.put( id.getUniqueId(), System.nanoTime() );
		}

		@Override
		public void executionFinished(TestIdentifier id, TestExecutionResult result) {
			Long t0 = starts.remove( id.getUniqueId() );
			if ( !id.isTest() ) {
				return;
			}
			long ms = t0 == null ? -1 : ( System.nanoTime() - t0 ) / 1_000_000;
			finished++;
			switch ( result.getStatus() ) {
				case SUCCESSFUL -> ok++;
				case FAILED -> failed++;
				case ABORTED -> aborted++;
			}
			System.out.println( "@@METHOD " + fqcn + " " + id.getDisplayName()
					+ " ms=" + ms + " " + result.getStatus() );
			result.getThrowable().ifPresent( t -> System.out.println(
					"    -> " + t.getClass().getName() + ": "
							+ String.valueOf( t.getMessage() ).split( "\n" )[0] ) );
			System.out.flush();
		}
	}
}
