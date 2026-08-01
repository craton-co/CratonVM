import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClass;

import java.lang.reflect.Type;
import java.util.ArrayList;
import java.util.List;

import org.hibernate.type.descriptor.java.IntegerJavaType;
import org.hibernate.type.descriptor.java.JavaType;
import org.hibernate.type.spi.TypeConfiguration;
import org.junit.platform.engine.TestExecutionResult;
import org.junit.platform.launcher.Launcher;
import org.junit.platform.launcher.LauncherDiscoveryRequest;
import org.junit.platform.launcher.TestExecutionListener;
import org.junit.platform.launcher.TestIdentifier;
import org.junit.platform.launcher.core.LauncherDiscoveryRequestBuilder;
import org.junit.platform.launcher.core.LauncherFactory;

/**
 * Reproduction driver for
 * {@code NoSuchMethodError: java/lang/Integer.getTypeName()} — see
 * {@code docs/known-issues/hibernate/gettypename-wrong-receiver-in-sessionfactory-rebuild-cascade-20260801.md}.
 *
 * <p>What the two field witnesses actually show (both logs are structurally
 * identical, line for line):
 *
 * <ol>
 * <li>the test body runs normally to ~11% (through
 *     {@code testNumericExpressionReturnTypes}), where it dies mid-method;</li>
 * <li>the factory is rebuilt three times, successfully;</li>
 * <li>from the SIXTH bootstrap on, every {@code JavaTypeRegistry} priming
 *     raises the warning and the bootstrap dies before it reaches the
 *     connection pool — 32 consecutive times, until the harness's wall cap
 *     kills the run.</li>
 * </ol>
 *
 * So the state that breaks priming is established by <em>running the suite</em>,
 * and once established it is permanent. Priming itself is not the variable:
 * {@code BaselinePrimeProbe} does 6000 primings in a fresh VM without a single
 * warning.
 *
 * <p>Reaching that state through the harness costs ~26s per sample, which is
 * why every hunt so far collected a handful. This probe pays for the suite once
 * and then takes samples at memory speed: each {@code new TypeConfiguration()}
 * runs {@code JavaTypeBaseline.prime()} against a real {@code JavaTypeRegistry},
 * i.e. exactly the per-bootstrap work, in microseconds.
 *
 * <p>It also prints every test failure with its stack trace, which
 * {@code CratonRunner} does not — the field logs never recorded what killed
 * {@code testNumericExpressionReturnTypes}.
 *
 * <p>usage: {@code PostSuitePrimeProbe <testClass> [primes] [rounds]}. With
 * {@code rounds > 1} the suite is re-run between priming batches, since the
 * field witness only broke on a LATE bootstrap.
 */
public final class PostSuitePrimeProbe {

	private static int runSuite(String className, int round) {
		final LauncherDiscoveryRequest request = LauncherDiscoveryRequestBuilder.request()
				.selectors( selectClass( className ) ).build();
		final Launcher launcher = LauncherFactory.create();
		final long[] counts = new long[2]; // found, ok
		launcher.registerTestExecutionListeners( new TestExecutionListener() {
			@Override
			public void executionFinished(TestIdentifier id, TestExecutionResult status) {
				if ( !id.isTest() ) {
					return;
				}
				counts[0]++;
				if ( status.getStatus() == TestExecutionResult.Status.SUCCESSFUL ) {
					counts[1]++;
				}
				else {
					System.out.println( "@@TESTFAIL round=" + round + " " + id.getDisplayName() );
					status.getThrowable().ifPresent( t -> t.printStackTrace( System.out ) );
				}
			}
		} );
		launcher.execute( request );
		System.out.println( "@@SUITE round=" + round + " found=" + counts[0] + " ok=" + counts[1]
				+ " failed=" + (counts[0] - counts[1]) );
		return (int) (counts[0] - counts[1]);
	}

	/** One bootstrap's worth of JavaTypeRegistry priming, checked. */
	private static String prime(int i) {
		try {
			final TypeConfiguration tc = new TypeConfiguration();
			final JavaType<?> found = tc.getJavaTypeRegistry().findDescriptor( Integer.class );
			if ( found != IntegerJavaType.INSTANCE ) {
				return "prime=" + i + " registry: Integer.class -> " + found;
			}
			final Type javaType = IntegerJavaType.INSTANCE.getJavaType();
			if ( javaType != Integer.class ) {
				return "prime=" + i + " IntegerJavaType.getJavaType() -> "
						+ ( javaType == null ? "null" : javaType.getClass().getName() + " (" + javaType + ")" );
			}
			return null;
		}
		catch (RuntimeException | Error e) {
			final java.io.StringWriter w = new java.io.StringWriter();
			e.printStackTrace( new java.io.PrintWriter( w ) );
			return "prime=" + i + " threw " + e.getClass().getName() + ": " + e.getMessage()
					+ System.lineSeparator() + w;
		}
	}

	public static void main(String[] args) {
		final String className = args.length > 0 ? args[0]
				: "org.hibernate.orm.test.hql.ASTParserLoadingTest";
		final int primes = args.length > 1 ? Integer.parseInt( args[1] ) : 20_000;
		final int rounds = args.length > 2 ? Integer.parseInt( args[2] ) : 2;

		final List<String> failures = new ArrayList<>();
		long done = 0;

		for ( int round = 0; round < rounds && failures.size() < 5; round++ ) {
			runSuite( className, round );
			for ( int i = 0; i < primes && failures.size() < 5; i++ ) {
				final String failure = prime( i );
				done++;
				if ( failure != null ) {
					failures.add( "round=" + round + " " + failure );
				}
			}
			System.out.println( "@@PRIMES round=" + round + " done=" + done
					+ " failures=" + failures.size() );
		}

		for ( String failure : failures ) {
			System.out.println( "POSTSUITEPRIME " + failure );
		}
		System.out.println( "@@POSTSUITEPRIME class=" + className + " rounds=" + rounds
				+ " primes=" + done + " failures=" + failures.size() );
		if ( !failures.isEmpty() ) {
			System.exit( 1 );
		}
	}
}
