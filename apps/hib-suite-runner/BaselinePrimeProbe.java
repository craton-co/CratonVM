import java.lang.reflect.Type;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ConcurrentHashMap;

import org.hibernate.type.descriptor.java.IntegerJavaType;
import org.hibernate.type.descriptor.java.JavaType;
import org.hibernate.type.descriptor.java.spi.JavaTypeBaseline;
import org.hibernate.type.spi.TypeConfiguration;

/**
 * Drives the work behind the {@code java/lang/Integer.getTypeName()} warning at
 * bootstrap rate instead of SessionFactory rate.
 *
 * <p>The warning names
 * {@code JavaTypeRegistry.addBaselineDescriptor(JavaType)V @pc=27}, and that
 * method runs exactly once per baseline registration, i.e. ~53 times per
 * {@code JavaTypeBaseline.prime()}, i.e. once per {@code TypeConfiguration},
 * i.e. once per SessionFactory bootstrap. Reaching it through a real bootstrap
 * costs ~26s a shot, which is why every hunt so far topped out at a handful of
 * samples. {@code new TypeConfiguration()} reaches the same code in
 * microseconds:
 *
 * <pre>
 * TypeConfiguration() -&gt; new JavaTypeRegistry(this) -&gt; JavaTypeBaseline.prime(this)
 * </pre>
 *
 * <p>Two arms, because they fail in different places:
 *
 * <ul>
 * <li><b>real</b> — construct real {@code TypeConfiguration}s. Exercises
 *     Hibernate's own {@code JavaTypeRegistry}, so it can trip the VM warning
 *     verbatim. It can only observe the defect through the resulting
 *     {@code NoSuchMethodError} (or a mis-keyed registry).</li>
 * <li><b>mirror</b> — a byte-for-byte structural clone of the two
 *     {@code addBaselineDescriptor} overloads, fed by the real
 *     {@code JavaTypeBaseline.prime()} over the real descriptor singletons. It
 *     can inspect what {@code descriptor.getJavaType()} actually returned, so
 *     it catches a wrong receiver even when the VM recovers and nothing
 *     propagates to Java.</li>
 * </ul>
 *
 * <p>usage: {@code BaselinePrimeProbe [iterations]} — prints
 * {@code @@BASELINEPRIME iters=N primes=P checks=C failures=M} and exits
 * non-zero on any failure.
 */
public final class BaselinePrimeProbe {

	/**
	 * Structural clone of {@code JavaTypeRegistry}'s two overloads. The shapes
	 * matter: the one-arg form must call {@code getJavaType()} through the
	 * {@code JavaType} interface and pass the result to a second method that
	 * calls {@code getTypeName()} on it, because the reported failure is at
	 * {@code @pc=27} — past the {@code invokevirtual} at 24 — which means the
	 * two-arg callee was inlined into the one-arg caller.
	 */
	static final class MirrorTarget implements JavaTypeBaseline.BaselineTarget {
		final ConcurrentHashMap<String, JavaType<?>> descriptorsByTypeName = new ConcurrentHashMap<>();
		final List<String> failures;

		MirrorTarget(List<String> failures) {
			this.failures = failures;
		}

		@Override
		public void addBaselineDescriptor(JavaType<?> descriptor) {
			final Type javaType = descriptor.getJavaType();
			if ( javaType == null ) {
				failures.add( descriptor.getClass().getName() + ".getJavaType() == null" );
				return;
			}
			// The check the real registry cannot make: every baseline descriptor
			// describes a Class, so anything else here is the wrong receiver,
			// caught BEFORE getTypeName() turns it into a NoSuchMethodError.
			if ( !(javaType instanceof Class) ) {
				failures.add( descriptor.getClass().getName() + ".getJavaType() returned a "
						+ javaType.getClass().getName() + " (" + javaType + "), not a Class" );
				return;
			}
			addBaselineDescriptor( javaType, descriptor );
		}

		@Override
		public void addBaselineDescriptor(Type describedJavaType, JavaType<?> descriptor) {
			descriptorsByTypeName.put( describedJavaType.getTypeName(), descriptor );
		}
	}

	public static void main(String[] args) {
		final int iterations = args.length > 0 ? Integer.parseInt( args[0] ) : 20_000;

		final List<String> failures = new ArrayList<>();
		long primes = 0, checks = 0;
		// The real bootstrap allocates hard while priming; keep a comparable
		// amount of garbage moving so the collector runs at a similar rate.
		final Object[] ballast = new Object[1024];

		for ( int i = 0; i < iterations && failures.size() < 20; i++ ) {
			ballast[i % ballast.length] = new byte[128 + (i & 1023)];

			// --- mirror arm ---
			final MirrorTarget mirror = new MirrorTarget( failures );
			JavaTypeBaseline.prime( mirror );
			primes++;
			checks += mirror.descriptorsByTypeName.size();
			if ( mirror.descriptorsByTypeName.get( "java.lang.Integer" ) != IntegerJavaType.INSTANCE ) {
				failures.add( "iter=" + i + " mirror registry: java.lang.Integer -> "
						+ mirror.descriptorsByTypeName.get( "java.lang.Integer" ) );
			}

			// --- real arm ---
			try {
				final TypeConfiguration tc = new TypeConfiguration();
				primes++;
				final JavaType<?> found = tc.getJavaTypeRegistry().findDescriptor( Integer.class );
				checks++;
				if ( found != IntegerJavaType.INSTANCE ) {
					failures.add( "iter=" + i + " real registry: Integer.class -> " + found );
				}
			}
			catch (RuntimeException | Error e) {
				failures.add( "iter=" + i + " new TypeConfiguration() threw "
						+ e.getClass().getName() + ": " + e.getMessage() );
			}

			if ( i > 0 && i % 2000 == 0 ) {
				System.out.println( "BASELINEPRIME progress iter=" + i + " primes=" + primes
						+ " failures=" + failures.size() );
			}
		}

		for ( String failure : failures ) {
			System.out.println( "BASELINEPRIME " + failure );
		}
		System.out.println( "@@BASELINEPRIME iters=" + iterations + " primes=" + primes
				+ " checks=" + checks + " failures=" + failures.size() );
		if ( !failures.isEmpty() ) {
			System.exit( 1 );
		}
	}
}
