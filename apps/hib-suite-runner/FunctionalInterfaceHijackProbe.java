import java.util.ArrayList;
import java.util.List;
import java.util.function.BiConsumer;
import java.util.function.BinaryOperator;
import java.util.function.Function;
import java.util.function.Supplier;

/**
 * Witness for the four functional interfaces CratonVM registers natives on.
 *
 * <p>`Collector.accumulator()` and friends hand back a synthetic object whose
 * class name IS the SAM interface, and the SAM natives that serve those objects
 * are registered on the public interface names — `java/util/function/Supplier.get`,
 * `BiConsumer.accept`, `Function.apply`, `BinaryOperator.apply`. Any dispatch
 * that resolves a SAM call by the constant-pool interface name rather than by
 * the receiver therefore runs the collector native on an ordinary user lambda.
 * None of those natives can decline: with no collector tag in field 0 they fall
 * into their catch-all arm and
 *
 * <ul>
 *   <li>{@code Supplier.get()} returns a fresh empty {@code ArrayList},</li>
 *   <li>{@code BiConsumer.accept(a, b)} calls {@code a.add(b)},</li>
 *   <li>{@code Function.apply(x)} returns {@code x} unchanged,</li>
 *   <li>{@code BinaryOperator.apply(a, b)} calls {@code a.addAll(b)}.</li>
 * </ul>
 *
 * <p>Three of those four are SILENT wrong answers — the lambda body simply never
 * runs. The fourth is the loud one that made this findable, in Hibernate's
 * {@code AbstractInitializer.startLoading}:
 * {@code NoSuchMethodError: <first argument's class>.add(Ljava/lang/Object;)Z}.
 *
 * <p>Two ingredients are needed, and the witness has both:
 * <ul>
 *   <li>the call site must be COMPILED — the interpreter resolves a lambda-proxy
 *       receiver through the proxy registry before native resolution, so it
 *       never reaches these natives;</li>
 *   <li>the site must be MEGAMORPHIC — a site that sees one proxy class stays on
 *       a cached path; only when several distinct lambdas share a call site does
 *       dispatch fall back to resolving by the constant-pool interface name.
 *       Hibernate's {@code forEachSubInitializer(BiConsumer, ...)} is called with
 *       {@code Initializer::startLoading}, {@code ::resolveKey},
 *       {@code ::initializeInstance} and more, which is exactly that shape.</li>
 * </ul>
 * Each interface below therefore gets one shared, hot dispatch method fed a
 * rotation of distinct lambdas.
 *
 * <p>usage: {@code FunctionalInterfaceHijackProbe [iterations]} — prints
 * {@code @@SAMHIJACK iters=N failures=M} and exits non-zero on any failure.
 */
public final class FunctionalInterfaceHijackProbe {

	private static final List<String> FAILURES = new ArrayList<>();

	/** A capturing box, so the lambda bodies have something observable to do. */
	private static final class Box {
		Object first;
		Object second;
		int hits;

		/** Target of the unbound method reference {@code Box::record}. */
		void record(Object value) {
			first = value;
			hits++;
		}
	}

	// ---- the shared, megamorphic dispatch sites -----------------------------

	private static Object dispatchSupplier(Supplier<?> supplier) {
		return supplier.get();
	}

	private static Object dispatchFunction(Function<Object, ?> function, Object argument) {
		return function.apply( argument );
	}

	private static void dispatchBiConsumer(BiConsumer<Object, Object> consumer, Object a, Object b) {
		consumer.accept( a, b );
	}

	private static Object dispatchBinaryOperator(BinaryOperator<Object> operator, Object a, Object b) {
		return operator.apply( a, b );
	}

	public static void main(String[] args) {
		final int iterations = args.length > 0 ? Integer.parseInt( args[0] ) : 200_000;

		// Distinct lambda classes per interface. Each `dispatch*` site above sees
		// every one of them, which is what drives it megamorphic.
		final List<Supplier<?>> suppliers = List.of(
				() -> "alpha", () -> "beta", () -> "gamma", () -> "delta", () -> "epsilon" );
		final List<Function<Object, ?>> functions = List.of(
				o -> "f1:" + o, o -> "f2:" + o, o -> "f3:" + o, o -> "f4:" + o, o -> "f5:" + o );
		final List<BinaryOperator<Object>> operators = List.of(
				(a, b) -> "o1:" + a + b, (a, b) -> "o2:" + a + b, (a, b) -> "o3:" + a + b,
				(a, b) -> "o4:" + a + b, (a, b) -> "o5:" + a + b );

		for ( int i = 0; i < iterations && FAILURES.size() < 20; i++ ) {
			checkSupplier( i, suppliers );
			checkFunction( i, functions );
			checkBinaryOperator( i, operators );
			checkBiConsumer( i );
		}

		for ( String failure : FAILURES ) {
			System.out.println( "SAMHIJACK " + failure );
		}
		System.out.println( "@@SAMHIJACK iters=" + iterations + " failures=" + FAILURES.size() );
		if ( !FAILURES.isEmpty() ) {
			System.exit( 1 );
		}
	}

	private static void checkSupplier(int iter, List<Supplier<?>> suppliers) {
		final int index = iter % suppliers.size();
		final String expected = new String[] { "alpha", "beta", "gamma", "delta", "epsilon" }[index];
		final Object produced;
		try {
			produced = dispatchSupplier( suppliers.get( index ) );
		}
		catch (RuntimeException | Error e) {
			FAILURES.add( "iter=" + iter + " Supplier.get threw " + e.getClass().getName() );
			return;
		}
		if ( !expected.equals( produced ) ) {
			FAILURES.add( "iter=" + iter + " Supplier.get returned "
					+ describe( produced ) + " instead of " + expected );
		}
	}

	private static void checkFunction(int iter, List<Function<Object, ?>> functions) {
		final int index = iter % functions.size();
		final String expected = "f" + ( index + 1 ) + ":in" + iter;
		final Object applied;
		try {
			applied = dispatchFunction( functions.get( index ), "in" + iter );
		}
		catch (RuntimeException | Error e) {
			FAILURES.add( "iter=" + iter + " Function.apply threw " + e.getClass().getName() );
			return;
		}
		if ( !expected.equals( applied ) ) {
			FAILURES.add( "iter=" + iter + " Function.apply returned "
					+ describe( applied ) + " instead of " + expected );
		}
	}

	private static void checkBinaryOperator(int iter, List<BinaryOperator<Object>> operators) {
		final int index = iter % operators.size();
		final String expected = "o" + ( index + 1 ) + ":" + iter + "x";
		final Object combined;
		try {
			combined = dispatchBinaryOperator( operators.get( index ), String.valueOf( iter ), "x" );
		}
		catch (RuntimeException | Error e) {
			FAILURES.add( "iter=" + iter + " BinaryOperator.apply threw " + e.getClass().getName() );
			return;
		}
		if ( !expected.equals( combined ) ) {
			FAILURES.add( "iter=" + iter + " BinaryOperator.apply returned "
					+ describe( combined ) + " instead of " + expected );
		}
	}

	/**
	 * The exact shape the Hibernate witness fails on: several distinct
	 * {@code BiConsumer}s — lambdas and an unbound instance-method reference —
	 * through one shared dispatch site, with the first SAM argument an object
	 * that has no {@code add(Object)} method.
	 */
	private static void checkBiConsumer(int iter) {
		final Box box = new Box();
		final String value = "value-" + iter;
		final BiConsumer<Object, Object> consumer = switch ( iter % 5 ) {
			case 0 -> (a, b) -> { ( (Box) a ).first = b; ( (Box) a ).hits++; };
			case 1 -> (a, b) -> { ( (Box) a ).second = b; ( (Box) a ).first = b; ( (Box) a ).hits++; };
			case 2 -> (a, b) -> { ( (Box) a ).first = b; ( (Box) a ).second = a; ( (Box) a ).hits++; };
			case 3 -> (a, b) -> { ( (Box) a ).hits++; ( (Box) a ).first = b; };
			default -> castingRecorder();
		};
		try {
			dispatchBiConsumer( consumer, box, value );
		}
		catch (RuntimeException | Error e) {
			FAILURES.add( "iter=" + iter + " BiConsumer.accept threw " + e.getClass().getName()
					+ ": " + e.getMessage() );
			return;
		}
		if ( box.hits != 1 || !value.equals( box.first ) ) {
			FAILURES.add( "iter=" + iter + " BiConsumer.accept did not run the lambda body (hits="
					+ box.hits + " first=" + describe( box.first ) + ")" );
		}
	}

	/** An unbound instance-method reference adapted to the erased SAM. */
	private static BiConsumer<Object, Object> castingRecorder() {
		final BiConsumer<Box, Object> typed = Box::record;
		return (a, b) -> typed.accept( (Box) a, b );
	}

	private static String describe(Object value) {
		return value == null ? "null" : value.getClass().getName() + "(" + value + ")";
	}
}
