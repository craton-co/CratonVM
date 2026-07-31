import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Collection-level witness for
 * {@code docs/known-issues/hibernate/hql-ordinal-parameter-dropped-under-jit-20260731.md}.
 *
 * <p>The reported failure is a parse that succeeds with the ordinal parameter
 * missing from {@code ParameterMetadataImpl}. Every step between the parse tree
 * and that metadata is a java.util collection operation that CratonVM services
 * with a native intrinsic, and any one of them answering wrongly drops the
 * parameter silently:
 *
 * <pre>
 *   SemanticQueryBuilder.resolveParameter   HashMap&lt;Object,?&gt;.putIfAbsent(Integer, p)
 *                                           -- a spurious non-null skips addParameter
 *   AbstractSqmStatement.addParameter       LinkedHashSet&lt;SqmParameter&gt;.add(p)
 *   AbstractSqmStatement.getSqmParameters   Collections.unmodifiableSet(set).isEmpty()
 *   ParameterMetadataImpl.&lt;init&gt;            HashMap&lt;Integer,?&gt;.put(position, qp)
 *   ParameterMetadataImpl.findQueryParameter  HashMap&lt;Integer,?&gt;.get(position)
 *   ParameterMetadataImpl.getOrdinalParameterLabels  keySet() iteration
 * </pre>
 *
 * <p>Driving those shapes directly runs millions of trials a minute instead of
 * the ~3/second the end-to-end Hibernate probe manages, which is the difference
 * between catching a 1-in-10000 event and not. The Integer-keyed maps here are
 * the exact shape CratonVM's {@code hm_int_fast} overlay claims
 * (fresh exact-class {@code HashMap} with boxed-Integer keys), and the mixed
 * Integer/String map is the one {@code resolveParameter} actually builds when a
 * query has both ordinal and named parameters.
 *
 * <p>usage: {@code ParamMapInvariantStress [iterations] [threads]} — prints
 * {@code @@MAPINVARIANT iters=N threads=T violations=M} and exits non-zero on
 * any violation. Returns normally on success so the VM's shutdown GC summary
 * is printed.
 */
public final class ParamMapInvariantStress {

	/**
	 * Stand-in for {@code SqmPositionalParameter} / {@code QueryParameterImpl}.
	 * The nullable {@code boxedPosition} / {@code name} pair mirrors the real
	 * classes, because how a parameter is CLASSIFIED is itself a candidate
	 * failure: {@code DomainParameterXref.fromSqm} calls a parameter named
	 * whenever {@code getName() != null}, and {@code ParameterMetadataImpl}
	 * files it under position only when {@code isOrdinal()} (i.e.
	 * {@code getPosition() != null}) holds. A positional parameter that reads
	 * back a non-null name, or a null position, lands in the named map and
	 * leaves the ordinal set empty — the reported symptom, with no exception
	 * anywhere.
	 */
	private static final class Param {
		final int position;
		final Integer boxedPosition;
		final String name;
		Param(int position, String name) {
			this.position = position;
			this.boxedPosition = name == null ? position : null;
			this.name = name;
		}
		boolean isOrdinal() { return boxedPosition != null; }
		boolean isNamed() { return name != null; }
	}

	/** Retained garbage, so the trials actually run against a collecting heap. */
	private static final int BALLAST_SLOTS = 512;

	public static void main(String[] args) throws Exception {
		final int iterations = args.length > 0 ? Integer.parseInt( args[0] ) : 200_000;
		final int threads = args.length > 1 ? Integer.parseInt( args[1] ) : 1;

		final List<List<String>> perThread = new ArrayList<>();
		final List<Thread> workers = new ArrayList<>();
		for ( int t = 0; t < threads; t++ ) {
			final List<String> violations = Collections.synchronizedList( new ArrayList<>() );
			perThread.add( violations );
			final int id = t;
			final Thread worker = new Thread( () -> run( id, iterations, violations ), "stress-" + t );
			workers.add( worker );
			worker.start();
		}
		for ( Thread worker : workers ) {
			worker.join();
		}

		int total = 0;
		for ( List<String> violations : perThread ) {
			for ( String line : violations ) {
				System.out.println( "MAPVIOLATION " + line );
			}
			total += violations.size();
		}
		System.out.println( "@@MAPINVARIANT iters=" + iterations + " threads=" + threads
				+ " violations=" + total );
		if ( total > 0 ) {
			System.exit( 1 );
		}
	}

	private static void run(int id, int iterations, List<String> violations) {
		// Ballast churns the young generation so the trials below straddle real
		// collections; without it a tight loop over small maps can run to
		// completion inside one nursery and never expose a GC-timing defect.
		final Object[] ballast = new Object[BALLAST_SLOTS];
		for ( int i = 0; i < iterations && violations.size() < 20; i++ ) {
			ballast[i % BALLAST_SLOTS] = new byte[64 + (i & 255)];
			ordinalResolveTrial( id, i, violations );
			mixedResolveTrial( id, i, violations );
			statementParameterSetTrial( id, i, violations );
			domainParameterXrefTrial( id, i, violations );
			metadataByPositionTrial( id, i, violations );
		}
		// Keep the ballast observably live to the end so it cannot be optimized
		// away wholesale.
		if ( ballast[iterations % BALLAST_SLOTS] == null && iterations > BALLAST_SLOTS ) {
			violations.add( "id=" + id + " impossible: ballast slot cleared" );
		}
	}

	/**
	 * {@code SemanticQueryBuilder.resolveParameter}, ordinal-only shape: a
	 * fresh {@code HashMap} keyed by the boxed position. The very first
	 * {@code putIfAbsent} for a position MUST report absence — that return is
	 * the sole gate on {@code parameterCollector.addParameter}.
	 */
	private static void ordinalResolveTrial(int id, int iter, List<String> violations) {
		final Map<Object, Param> parameters = new HashMap<>();
		final int count = 1 + (iter & 3);
		for ( int position = 1; position <= count; position++ ) {
			final Param param = new Param( position, null );
			final Param existing = parameters.putIfAbsent( position, param );
			if ( existing != null ) {
				violations.add( "id=" + id + " iter=" + iter + " ordinalResolve: putIfAbsent(" + position
						+ ") on a fresh map returned existing position=" + existing.position );
				return;
			}
			// The second call must find exactly what the first stored.
			final Param again = parameters.putIfAbsent( position, new Param( position, null ) );
			if ( again != param ) {
				violations.add( "id=" + id + " iter=" + iter + " ordinalResolve: re-putIfAbsent(" + position
						+ ") returned " + (again == null ? "null" : "a different Param") );
				return;
			}
		}
		if ( parameters.size() != count ) {
			violations.add( "id=" + id + " iter=" + iter + " ordinalResolve: size=" + parameters.size()
					+ " expected=" + count );
		}
	}

	/**
	 * Same call site, mixed shape: ordinal and named parameters share one
	 * {@code HashMap<Object,?>}, so it holds both Integer and String keys. That
	 * heterogeneity is what forces CratonVM's integer overlay to hand the map
	 * back to the ordinary node path mid-life.
	 */
	private static void mixedResolveTrial(int id, int iter, List<String> violations) {
		final Map<Object, Param> parameters = new HashMap<>();
		final Object[] keys = { 1, "first", 2, "n", 3 };
		for ( Object key : keys ) {
			final Param param = new Param( key instanceof Integer i ? i : -1,
					key instanceof String s ? s : null );
			if ( parameters.putIfAbsent( key, param ) != null ) {
				violations.add( "id=" + id + " iter=" + iter + " mixedResolve: putIfAbsent(" + key
						+ ") on a fresh map reported present" );
				return;
			}
		}
		for ( Object key : keys ) {
			if ( parameters.get( key ) == null ) {
				violations.add( "id=" + id + " iter=" + iter + " mixedResolve: get(" + key
						+ ") lost its entry; size=" + parameters.size() );
				return;
			}
		}
		if ( parameters.size() != keys.length ) {
			violations.add( "id=" + id + " iter=" + iter + " mixedResolve: size=" + parameters.size()
					+ " expected=" + keys.length );
		}
	}

	/**
	 * {@code AbstractSqmStatement.addParameter} / {@code getSqmParameters}: a
	 * lazily created {@code LinkedHashSet} read back through
	 * {@code Collections.unmodifiableSet}. An empty answer here is what turns
	 * into {@code ParameterMetadataImpl.EMPTY}.
	 */
	private static void statementParameterSetTrial(int id, int iter, List<String> violations) {
		Set<Param> parameters = null;
		final int count = 1 + (iter & 1);
		for ( int position = 1; position <= count; position++ ) {
			if ( parameters == null ) {
				parameters = new LinkedHashSet<>();
			}
			parameters.add( new Param( position, null ) );
		}
		final Set<Param> view = Collections.unmodifiableSet( parameters );
		if ( view.isEmpty() ) {
			violations.add( "id=" + id + " iter=" + iter
					+ " statementParameters: unmodifiable view of a " + count + "-element set is empty" );
		}
		else if ( view.size() != count ) {
			violations.add( "id=" + id + " iter=" + iter + " statementParameters: view size="
					+ view.size() + " expected=" + count );
		}
		else {
			int seen = 0;
			for ( Param ignored : view ) {
				seen++;
			}
			if ( seen != count ) {
				violations.add( "id=" + id + " iter=" + iter + " statementParameters: iterated "
						+ seen + " of " + count );
			}
		}
	}

	/**
	 * {@code DomainParameterXref}: the statement's parameter set is folded into
	 * a {@code LinkedHashMap} keyed by query parameter (via
	 * {@code computeIfAbsent}) and an {@code IdentityHashMap} keyed by SQM
	 * parameter. The {@code LinkedHashMap} is what
	 * {@code ParameterMetadataImpl} then iterates to classify each parameter as
	 * ordinal or named, so an entry lost here reaches the caller as an ordinal
	 * parameter that was never declared.
	 */
	private static void domainParameterXrefTrial(int id, int iter, List<String> violations) {
		final int count = 1 + (iter & 3);
		final Map<Param, List<Param>> sqmParamsByQueryParam = new java.util.LinkedHashMap<>( count );
		final Map<Param, Param> queryParamBySqmParam = new java.util.IdentityHashMap<>( count );
		final List<Param> sqmParams = new ArrayList<>();
		for ( int position = 1; position <= count; position++ ) {
			final Param sqmParam = new Param( position, null );
			final Param queryParam = new Param( position, null );
			sqmParams.add( sqmParam );
			sqmParamsByQueryParam.computeIfAbsent( queryParam, k -> new ArrayList<>() ).add( sqmParam );
			queryParamBySqmParam.put( sqmParam, queryParam );
		}
		if ( sqmParamsByQueryParam.isEmpty() ) {
			violations.add( "id=" + id + " iter=" + iter + " xref: LinkedHashMap of " + count
					+ " query parameters reads empty" );
			return;
		}
		int seen = 0;
		for ( Map.Entry<Param, List<Param>> entry : sqmParamsByQueryParam.entrySet() ) {
			seen++;
			if ( entry.getValue().size() != 1 ) {
				violations.add( "id=" + id + " iter=" + iter + " xref: query parameter position="
						+ entry.getKey().position + " has " + entry.getValue().size() + " SQM parameters" );
				return;
			}
		}
		if ( seen != count ) {
			violations.add( "id=" + id + " iter=" + iter + " xref: iterated " + seen + " of " + count
					+ " query parameters (size=" + sqmParamsByQueryParam.size() + ")" );
			return;
		}
		for ( Param sqmParam : sqmParams ) {
			if ( queryParamBySqmParam.get( sqmParam ) == null ) {
				violations.add( "id=" + id + " iter=" + iter
						+ " xref: IdentityHashMap lost SQM parameter position=" + sqmParam.position
						+ "; size=" + queryParamBySqmParam.size() );
				return;
			}
		}
	}

	/**
	 * {@code ParameterMetadataImpl}: the ordinal parameters land in a fresh
	 * {@code HashMap<Integer,?>} which is then read back both by key
	 * ({@code findQueryParameter}) and by {@code keySet()} (the "[%s]" in the
	 * failure message). The reported symptom is precisely those two disagreeing
	 * with what was put in.
	 */
	private static void metadataByPositionTrial(int id, int iter, List<String> violations) {
		final int count = 1 + (iter & 3);
		final Map<Integer, Param> byPosition = new HashMap<>();
		for ( int position = 1; position <= count; position++ ) {
			final Param queryParameter = new Param( position, null );
			// The classification branch ParameterMetadataImpl actually takes.
			// A positional parameter reading back as named is a silent route
			// into the named map, leaving the ordinal map null.
			if ( !queryParameter.isOrdinal() || queryParameter.isNamed() ) {
				violations.add( "id=" + id + " iter=" + iter + " metadataByPosition: parameter "
						+ position + " classified ordinal=" + queryParameter.isOrdinal()
						+ " named=" + queryParameter.isNamed() );
				return;
			}
			byPosition.put( queryParameter.boxedPosition, queryParameter );
		}
		final StringBuilder labels = new StringBuilder();
		for ( Integer label : byPosition.keySet() ) {
			labels.append( labels.length() == 0 ? "" : ", " ).append( label );
		}
		if ( labels.length() == 0 ) {
			violations.add( "id=" + id + " iter=" + iter + " metadataByPosition: keySet() of a "
					+ count + "-entry map is empty; size=" + byPosition.size() );
			return;
		}
		for ( int position = 1; position <= count; position++ ) {
			final Param found = byPosition.get( position );
			if ( found == null ) {
				violations.add( "id=" + id + " iter=" + iter + " metadataByPosition: get(" + position
						+ ") null; labels=[" + labels + "] size=" + byPosition.size() );
				return;
			}
			if ( found.position != position ) {
				violations.add( "id=" + id + " iter=" + iter + " metadataByPosition: get(" + position
						+ ") returned position=" + found.position );
				return;
			}
		}
	}
}
