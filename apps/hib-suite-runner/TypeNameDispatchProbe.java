import java.lang.reflect.Type;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * Witness for a wrong-receiver dispatch of {@code java.lang.reflect.Type.getTypeName()}.
 *
 * <p>Observed in Hibernate's {@code JavaTypeRegistry.addBaselineDescriptor}, whose
 * two-argument overload does
 * {@code descriptorsByTypeName.put(describedJavaType.getTypeName(), descriptor)}
 * where {@code describedJavaType} is almost always a {@code Class} mirror:
 *
 * <pre>
 * NoSuchMethodError: java/lang/Integer.getTypeName()Ljava/lang/String;
 *   caller=JavaTypeRegistry.addBaselineDescriptor(..)V @pc=27
 * </pre>
 *
 * <p>The receiver is a {@code Class} — which implements {@code Type} — so resolving
 * the call against {@code java/lang/Integer}, the class the mirror DESCRIBES rather
 * than the class the mirror IS, is the defect. Two properties of the field report
 * shape this probe:
 *
 * <ul>
 *   <li>it is <b>sticky</b> — once a process starts producing it, every later
 *       {@code SessionFactory} build produces it again, ~32 times across a run,
 *       from ~2.5 minutes in to the end. That says a compiled call site or a
 *       resolution cache is poisoned, not that a single call went wrong;</li>
 *   <li>the receiver varies over many different {@code Class} mirrors
 *       ({@code Integer.class}, {@code String.class}, …) through ONE call site,
 *       which is monomorphic in the JVM's terms (every receiver is a
 *       {@code java.lang.Class}) but would look megamorphic to any cache keyed on
 *       the mirror's described class instead.</li>
 * </ul>
 *
 * <p>So the shape below is: one shared, hot, statically-typed-as-{@code Type} call
 * site, fed a long rotation of distinct {@code Class} mirrors, with the result used
 * as a map key exactly as Hibernate uses it. Mirrors of primitives, arrays and
 * generics are included because each takes a different path to a mirror object.
 *
 * <p>usage: {@code TypeNameDispatchProbe [iterations]} — prints
 * {@code @@TYPENAME iters=N checks=C failures=M} and exits non-zero on any failure.
 */
public final class TypeNameDispatchProbe {

	/** Every receiver is a {@code java.lang.Class}; every DESCRIBED class differs. */
	private static final Class<?>[] MIRRORS = {
		Integer.class, String.class, Long.class, Double.class, Boolean.class,
		Byte.class, Short.class, Character.class, Float.class, Object.class,
		int.class, long.class, double.class, boolean.class, char.class,
		int[].class, String[].class, Object[].class, int[][].class,
		java.math.BigDecimal.class, java.math.BigInteger.class,
		java.util.Date.class, java.util.UUID.class, java.util.Locale.class,
		java.time.Instant.class, java.time.LocalDate.class, java.time.Duration.class,
		java.util.List.class, java.util.Map.class, java.util.ArrayList.class,
	};

	private static final String[] EXPECTED = new String[MIRRORS.length];
	static {
		for ( int i = 0; i < MIRRORS.length; i++ ) {
			EXPECTED[i] = MIRRORS[i].getTypeName();
		}
	}

	/**
	 * The shared dispatch site, statically typed as {@code Type} so the call is an
	 * {@code invokeinterface} on {@code java/lang/reflect/Type} — exactly what
	 * {@code addBaselineDescriptor} compiles to.
	 */
	private static String typeNameOf(Type type) {
		return type.getTypeName();
	}

	public static void main(String[] args) {
		final int iterations = args.length > 0 ? Integer.parseInt( args[0] ) : 200_000;

		final List<String> failures = new ArrayList<>();
		long checks = 0;
		// Retained churn, so the trials straddle real collections.
		final Object[] ballast = new Object[512];

		for ( int i = 0; i < iterations && failures.size() < 20; i++ ) {
			ballast[i % ballast.length] = new byte[64 + (i & 255)];
			// A fresh registry per iteration, mirroring one SessionFactory build.
			final Map<String, Object> descriptorsByTypeName = new HashMap<>();
			for ( int m = 0; m < MIRRORS.length; m++ ) {
				checks++;
				final String name;
				try {
					name = typeNameOf( MIRRORS[m] );
				}
				catch (RuntimeException | Error e) {
					failures.add( "iter=" + i + " " + MIRRORS[m] + " threw "
							+ e.getClass().getName() + ": " + e.getMessage() );
					continue;
				}
				if ( !EXPECTED[m].equals( name ) ) {
					failures.add( "iter=" + i + " expected=" + EXPECTED[m] + " got=" + name );
					continue;
				}
				descriptorsByTypeName.put( name, MIRRORS[m] );
			}
			if ( descriptorsByTypeName.size() != MIRRORS.length ) {
				failures.add( "iter=" + i + " registry size=" + descriptorsByTypeName.size()
						+ " expected=" + MIRRORS.length );
			}
		}

		for ( String failure : failures ) {
			System.out.println( "TYPENAME " + failure );
		}
		System.out.println( "@@TYPENAME iters=" + iterations + " checks=" + checks
				+ " failures=" + failures.size() );
		if ( !failures.isEmpty() ) {
			System.exit( 1 );
		}
	}
}
