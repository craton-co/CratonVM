import java.lang.reflect.Type;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

import org.hibernate.type.descriptor.java.BooleanJavaType;
import org.hibernate.type.descriptor.java.ByteJavaType;
import org.hibernate.type.descriptor.java.CharacterJavaType;
import org.hibernate.type.descriptor.java.DoubleJavaType;
import org.hibernate.type.descriptor.java.FloatJavaType;
import org.hibernate.type.descriptor.java.IntegerJavaType;
import org.hibernate.type.descriptor.java.JavaType;
import org.hibernate.type.descriptor.java.LongJavaType;
import org.hibernate.type.descriptor.java.ShortJavaType;
import org.hibernate.type.descriptor.java.StringJavaType;

/**
 * Reduction of the two-instruction sequence behind
 * {@code NoSuchMethodError: java/lang/Integer.getTypeName()}.
 *
 * <p>{@code JavaTypeRegistry.addBaselineDescriptor} is, in bytecode:
 *
 * <pre>
 *  1: invokeinterface JavaType.getJavaType:()Ljava/lang/reflect/Type;   // -&gt; local 2
 * 24: invokevirtual   addBaselineDescriptor:(Type;JavaType;)V           // inlined
 *       -&gt; invokeinterface Type.getTypeName:()Ljava/lang/String;        // on local 2
 * </pre>
 *
 * The reported receiver class for that last call is {@code java/lang/Integer}.
 * A {@code Class} mirror is a {@code java.lang.Class}, never a
 * {@code java.lang.Integer}, so either {@code getJavaType()} handed back
 * something that is not the mirror, or local 2 stopped pointing at it. This
 * probe pins down the first half.
 *
 * <p>{@code AbstractClassJavaType.getJavaType()} is a trivial accessor returning
 * the {@code Class<T> type} field. {@code IntegerJavaType} also declares
 * {@code public static final Integer ZERO}, and the failure names Integer every
 * single time across two independent runs — never String, never Long — which is
 * what makes a field/slot mix-up on THIS class worth testing directly rather
 * than inferring.
 *
 * <p>Both call shapes are driven: through a shared site typed as the
 * {@code JavaType} interface (what the real caller compiles to, and what goes
 * megamorphic over the descriptor implementations), and directly on the concrete
 * type. The result is used as a map key via {@code getTypeName()}, as the real
 * caller does, so a wrong receiver surfaces the same way.
 *
 * <p>usage: {@code JavaTypeAccessorProbe [iterations]} — prints
 * {@code @@JAVATYPEACC iters=N checks=C failures=M} and exits non-zero on any
 * failure.
 */
public final class JavaTypeAccessorProbe {

	private static final JavaType<?>[] DESCRIPTORS = {
		IntegerJavaType.INSTANCE, StringJavaType.INSTANCE, LongJavaType.INSTANCE,
		ShortJavaType.INSTANCE, ByteJavaType.INSTANCE, DoubleJavaType.INSTANCE,
		FloatJavaType.INSTANCE, BooleanJavaType.INSTANCE, CharacterJavaType.INSTANCE,
	};

	private static final Class<?>[] EXPECTED = {
		Integer.class, String.class, Long.class,
		Short.class, Byte.class, Double.class,
		Float.class, Boolean.class, Character.class,
	};

	/** The interface-typed call site, exactly as `addBaselineDescriptor` has it. */
	private static Type javaTypeOf(JavaType<?> descriptor) {
		return descriptor.getJavaType();
	}

	/** The inlined callee's body: `descriptorsByTypeName.put(t.getTypeName(), d)`. */
	private static void register(Map<String, Object> registry, Type describedJavaType, Object descriptor) {
		registry.put( describedJavaType.getTypeName(), descriptor );
	}

	public static void main(String[] args) {
		final int iterations = args.length > 0 ? Integer.parseInt( args[0] ) : 200_000;

		final List<String> failures = new ArrayList<>();
		long checks = 0;
		final Object[] ballast = new Object[512];

		for ( int i = 0; i < iterations && failures.size() < 20; i++ ) {
			ballast[i % ballast.length] = new byte[64 + (i & 255)];
			final Map<String, Object> registry = new HashMap<>();
			for ( int d = 0; d < DESCRIPTORS.length; d++ ) {
				checks++;
				final Type javaType;
				try {
					javaType = javaTypeOf( DESCRIPTORS[d] );
				}
				catch (RuntimeException | Error e) {
					failures.add( "iter=" + i + " " + DESCRIPTORS[d].getClass().getSimpleName()
							+ ".getJavaType() threw " + e.getClass().getName() );
					continue;
				}
				if ( javaType != EXPECTED[d] ) {
					failures.add( "iter=" + i + " " + DESCRIPTORS[d].getClass().getSimpleName()
							+ ".getJavaType() returned "
							+ ( javaType == null ? "null"
									: javaType.getClass().getName() + " (" + javaType + ")" )
							+ " expected " + EXPECTED[d] );
					continue;
				}
				try {
					register( registry, javaType, DESCRIPTORS[d] );
				}
				catch (RuntimeException | Error e) {
					failures.add( "iter=" + i + " register(" + EXPECTED[d] + ") threw "
							+ e.getClass().getName() + ": " + e.getMessage() );
				}
			}
			if ( registry.size() != DESCRIPTORS.length ) {
				failures.add( "iter=" + i + " registry size=" + registry.size()
						+ " expected=" + DESCRIPTORS.length );
			}
		}

		for ( String failure : failures ) {
			System.out.println( "JAVATYPEACC " + failure );
		}
		System.out.println( "@@JAVATYPEACC iters=" + iterations + " checks=" + checks
				+ " failures=" + failures.size() );
		if ( !failures.isEmpty() ) {
			System.exit( 1 );
		}
	}
}
