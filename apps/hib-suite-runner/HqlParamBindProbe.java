import java.util.ArrayList;
import java.util.List;

import org.hibernate.SessionFactory;
import org.hibernate.cfg.Configuration;
import org.hibernate.query.Query;

/**
 * Downstream witness for
 * {@code docs/known-issues/hibernate/hql-ordinal-parameter-dropped-under-jit-20260731.md}.
 *
 * <p>{@code HqlParseStress} proves the ANTLR parse tree keeps every parameter
 * marker. It cannot see the defect this probe hunts, which lives *after* the
 * parse: the marker is in the tree, but the {@code SqmParameter} never reaches
 * the statement's parameter set, so Hibernate reports
 * {@code No parameter labelled '?1' in query with ordinal parameters []} when
 * the caller then binds it.
 *
 * <p>The path exercised is exactly the one the witness test takes —
 * {@code Session.createQuery(String, Class)} ->
 * {@code QueryInterpretationCacheStandardImpl.resolveHqlInterpretation} ->
 * {@code SemanticQueryBuilder.resolveParameter} (a {@code HashMap.putIfAbsent}
 * keyed on the boxed ordinal) -> {@code AbstractSqmStatement.addParameter} (a
 * {@code LinkedHashSet.add}) -> {@code ParameterMetadataImpl}. Every one of
 * those steps runs on a CratonVM native collection intrinsic, and a single
 * spurious "already present" answer from the map silently drops the parameter
 * without any syntax error.
 *
 * <p>Two arms per iteration, because the two failure shapes are different:
 * <ul>
 *   <li><b>fresh</b> — a query string unique to the iteration, so the
 *       interpretation cache always misses and the full SQM build reruns. This
 *       is the arm that can catch a dropped {@code addParameter}.</li>
 *   <li><b>cached</b> — a fixed query string, so the interpretation cache is hit
 *       from the second iteration on. This is the arm that can catch the cache
 *       handing back another query's interpretation.</li>
 * </ul>
 *
 * <p>usage: {@code HqlParamBindProbe [iterations]} — prints
 * {@code @@PARAMPROBE iters=N checks=C drops=M} and exits non-zero on any drop.
 * Returns normally on success so the VM's shutdown GC summary is printed.
 */
public final class HqlParamBindProbe {

	/**
	 * Query shapes, each with the parameters it must expose. {@code %d} (when
	 * present) is filled with the iteration number to defeat the interpretation
	 * cache; the shape without it is the cached arm.
	 */
	private record Shape(String template, boolean fresh, Object[] ordinalValues, Object[] nameValues) {
		int ordinalCount() { return ordinalValues.length; }
		int nameCount() { return nameValues.length / 2; }
	}

	private static final Shape[] SHAPES = {
		// The witness query itself, verbatim (cached arm), bound with null
		// exactly as the test does.
		new Shape( "from Human where ?1 is null", false,
				new Object[] { null }, new Object[0] ),
		// ... and made unique per iteration so the SQM build reruns every time.
		new Shape( "from Human where ?1 is null and bodyWeight <> %d", true,
				new Object[] { null }, new Object[0] ),
		// Two ordinals, so a drop of either is visible. Bound with real values:
		// a comparison operand needs an inferable type.
		new Shape( "from Animal a where a.bodyWeight > ?1 and a.description = ?2 and a.id <> %d",
				true, new Object[] { 1.0f, "x" }, new Object[0] ),
		// Named parameters take the same resolveParameter map with String keys.
		new Shape( "from Human h where h.nickName = :nick and h.intValue > :n and h.bodyWeight <> %d",
				true, new Object[0], new Object[] { "nick", "x", "n", 1 } ),
		// Mixed shapes are what make the resolveParameter map heterogeneous
		// (Integer and String keys in one HashMap).
		new Shape( "from Human h where h.nickName = :nick and h.intValue <> %d", true,
				new Object[0], new Object[] { "nick", "x" } ),
		new Shape( "from Human where ?1 is null and ?2 is null", false,
				new Object[] { null, null }, new Object[0] ),
	};

	public static void main(String[] args) {
		final int iterations = args.length > 0 ? Integer.parseInt( args[0] ) : 2000;

		final Configuration cfg = new Configuration();
		cfg.addResource( "org/hibernate/orm/test/hql/Animal.hbm.xml" );
		cfg.setProperty( "hibernate.dialect", "org.hibernate.dialect.H2Dialect" );
		cfg.setProperty( "hibernate.connection.driver_class", "org.h2.Driver" );
		cfg.setProperty( "hibernate.connection.url", "jdbc:h2:mem:hqlparamprobe;DB_CLOSE_DELAY=-1" );
		cfg.setProperty( "hibernate.connection.username", "sa" );
		cfg.setProperty( "hibernate.connection.password", "" );
		cfg.setProperty( "hibernate.hbm2ddl.auto", "create-drop" );
		cfg.setProperty( "hibernate.show_sql", "false" );
		// The witness class runs under these two (see its @ServiceRegistry), and
		// both change the code path this probe is aiming at: statistics adds the
		// timing/counter branches inside
		// `QueryInterpretationCacheStandardImpl.resolveHqlInterpretation`, and the
		// query cache puts a second, differently-keyed cache in front of the
		// interpretation cache. Running without them probes a strictly narrower
		// path than the failure was observed on.
		cfg.setProperty( "hibernate.generate_statistics", "true" );
		cfg.setProperty( "hibernate.cache.use_query_cache", "true" );

		final List<String> drops = new ArrayList<>();
		long checks = 0;

		try ( SessionFactory sf = cfg.buildSessionFactory() ) {
			outer:
			for ( int i = 0; i < iterations; i++ ) {
				try ( var session = sf.openSession() ) {
					for ( Shape shape : SHAPES ) {
						final String hql = shape.fresh()
								? String.format( shape.template(), i )
								: shape.template();
						checks++;
						final String failure = check( session, hql, shape );
						if ( failure != null ) {
							drops.add( "iter=" + i + " " + failure + " query=" + hql );
							if ( drops.size() >= 20 ) {
								break outer;
							}
						}
					}
				}
			}
		}

		for ( String line : drops ) {
			System.out.println( "PARAMDROP " + line );
		}
		System.out.println( "@@PARAMPROBE iters=" + iterations + " checks=" + checks
				+ " drops=" + drops.size() );
		if ( !drops.isEmpty() ) {
			System.exit( 1 );
		}
	}

	/**
	 * Build the query and bind every parameter the source declares. Returns
	 * {@code null} when all of them bound, or a short description otherwise.
	 *
	 * <p>The binding call is the real check: {@code setParameter} is what throws
	 * {@code No parameter labelled '?1' in query with ordinal parameters []}.
	 * The declared-count assertion in front of it localises a drop to the SQM
	 * build rather than to the binding itself.
	 */
	private static String check(org.hibernate.Session session, String hql, Shape shape) {
		final Query<?> query;
		try {
			query = session.createQuery( hql, Object.class );
		}
		catch (RuntimeException e) {
			return "createQuery=" + e.getClass().getSimpleName() + ":" + e.getMessage();
		}
		final int expected = shape.ordinalCount() + shape.nameCount();
		final int declared = query.getParameterMetadata().getParameterCount();
		if ( declared != expected ) {
			return "declared=" + declared + " expected=" + expected;
		}
		for ( int i = 0; i < shape.ordinalCount(); i++ ) {
			try {
				query.setParameter( i + 1, shape.ordinalValues()[i] );
			}
			catch (RuntimeException e) {
				return "bindOrdinal=" + (i + 1) + " " + e.getClass().getSimpleName() + ":" + e.getMessage();
			}
		}
		for ( int i = 0; i < shape.nameCount(); i++ ) {
			final String name = (String) shape.nameValues()[2 * i];
			try {
				query.setParameter( name, shape.nameValues()[2 * i + 1] );
			}
			catch (RuntimeException e) {
				return "bindName=" + name + " " + e.getClass().getSimpleName() + ":" + e.getMessage();
			}
		}
		// Execute, as the witness test does. Everything past `setParameter` —
		// SQM-to-SQL translation, the query-plan cache, JDBC parameter binding —
		// re-reads the same parameter metadata, so a drop that survives the
		// binding call can still surface here.
		try {
			query.getResultList();
		}
		catch (RuntimeException e) {
			return "list=" + e.getClass().getSimpleName() + ":" + e.getMessage();
		}
		return null;
	}
}
