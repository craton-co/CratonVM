import java.sql.Connection;
import java.sql.DriverManager;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

import jakarta.persistence.Embeddable;
import jakarta.persistence.Embedded;
import jakarta.persistence.Entity;
import jakarta.persistence.EnumType;
import jakarta.persistence.Enumerated;
import jakarta.persistence.Id;
import jakarta.persistence.Table;

import org.hibernate.Session;
import org.hibernate.SessionFactory;
import org.hibernate.Transaction;
import org.hibernate.cfg.Configuration;
import org.hibernate.query.Query;

/**
 * Stand-alone throughput probe for
 * {@code org.hibernate.orm.test.sql.exec.SmokeTests#testQueryConcurrency}.
 *
 * <p>The real test runs 50 forks x 400 iterations on a 5-thread pool and is
 * capped by Hibernate's 120 s per-method JUnit limit, so a single data point
 * costs ten minutes and cannot be decomposed. This runs the <em>identical</em>
 * unit of work — the same three HQL queries against the same two annotated
 * entities on the same in-memory H2 — with the fork/iteration/thread counts on
 * the command line and a warm-up excluded from the timing, so one data point
 * costs about 90 s and the cost can be attributed to a layer.
 *
 * <p>usage: {@code SmokeConcProbe [forks] [iterations] [threads] [warmupForks] [mode]}
 *
 * <p>Modes, cheapest first — each runs the same fork/iteration/thread structure
 * so their timings are directly comparable:
 * <ul>
 *   <li>{@code txn} — open a session, begin, commit, close. No statement.</li>
 *   <li>{@code session} — open and close a session, no transaction, no JDBC.</li>
 *   <li>{@code jdbc} — raw H2 connect / begin / commit / close, no Hibernate.</li>
 *   <li>{@code parse} — three {@code createQuery} calls, nothing executed.</li>
 *   <li>{@code exec} — one query built and executed.</li>
 *   <li>{@code full} — the real test body: three queries, all executed. Default.</li>
 * </ul>
 *
 * <p>Prints {@code @@SMOKECONC ... PROJECTED_TEST_MS=n}, the extrapolation of
 * the measured steady-state rate to the real test's 50x400 body, which is the
 * number to compare against the 120 000 ms cap and against HotSpot.
 */
public final class SmokeConcProbe {

	private static final int REAL_FORKS = 50;
	private static final int REAL_ITERATIONS = 400;

	public static void main(String[] args) throws Exception {
		final int forks = args.length > 0 ? Integer.parseInt( args[0] ) : 6;
		final int iterations = args.length > 1 ? Integer.parseInt( args[1] ) : 400;
		final int threads = args.length > 2 ? Integer.parseInt( args[2] ) : 5;
		final int warmupForks = args.length > 3 ? Integer.parseInt( args[3] ) : 2;
		final String mode = args.length > 4 ? args[4] : "full";

		final Configuration cfg = new Configuration();
		cfg.addAnnotatedClass( SimpleEntity.class );
		cfg.addAnnotatedClass( OtherEntity.class );
		cfg.setProperty( "hibernate.dialect", "org.hibernate.dialect.H2Dialect" );
		cfg.setProperty( "hibernate.connection.driver_class", "org.h2.Driver" );
		cfg.setProperty( "hibernate.connection.url", JDBC_URL );
		cfg.setProperty( "hibernate.connection.username", "sa" );
		cfg.setProperty( "hibernate.connection.password", "" );
		cfg.setProperty( "hibernate.hbm2ddl.auto", "create-drop" );
		cfg.setProperty( "hibernate.show_sql", "false" );
		// The real fixture disables the second-level cache; leaving it on would
		// make every query after the first a cache hit and measure nothing.
		cfg.setProperty( "hibernate.cache.use_second_level_cache", "false" );
		cfg.setProperty( "hibernate.cache.use_query_cache", "false" );
		cfg.setProperty( "hibernate.connection.pool_size", String.valueOf( threads * 2 ) );

		try ( SessionFactory sf = cfg.buildSessionFactory() ) {
			seed( sf );

			final Runnable unit = unitFor( mode, sf );

			// Warm-up: same work, excluded from the timing, so class loading,
			// SQL-statement caching and any JIT tier-up have already happened
			// when the measured window opens. Without it the first fork carries
			// the whole bootstrap and the rate reads far below steady state.
			runForks( warmupForks, iterations, threads, unit );

			ROWS.set( 0 );
			final long start = System.nanoTime();
			runForks( forks, iterations, threads, unit );
			final long elapsedMs = ( System.nanoTime() - start ) / 1_000_000L;

			final long units = (long) forks * iterations;
			final double perUnitMs = elapsedMs / (double) units;
			final long projected =
					Math.round( perUnitMs * REAL_FORKS * REAL_ITERATIONS );
			final long rate = elapsedMs == 0 ? -1 : units * 1000L / elapsedMs;

			final long rows = ROWS.get();
			final long expectedRows =
					"full".equals( mode ) ? units * 2 : "exec".equals( mode ) ? units : 0;

			System.out.printf(
					"@@SMOKECONC mode=%s forks=%d iterations=%d threads=%d warmup=%d "
							+ "units=%d ms=%d units_per_s=%d rows=%d PROJECTED_TEST_MS=%d%n",
					mode, forks, iterations, threads, warmupForks,
					units, elapsedMs, rate, rows, projected );

			if ( rows != expectedRows ) {
				System.out.printf(
						"@@SMOKECONC-INVALID mode=%s returned %d rows, expected %d — "
								+ "the timing above measured something other than this workload%n",
						mode, rows, expectedRows );
				System.exit( 1 );
			}
		}
	}

	private static final String JDBC_URL =
			"jdbc:h2:mem:smokeconc;DB_CLOSE_DELAY=-1";

	/**
	 * Rows actually returned by the executing modes. The seed row makes each
	 * {@code full} unit worth exactly two rows (the {@code b1} query matches
	 * nothing by design, exactly as in the fixture) and each {@code exec} unit
	 * one. Checked against the unit count at the end: a run whose queries all
	 * returned nothing is orders of magnitude cheaper than the workload this
	 * probe claims to measure, and would otherwise be reported as a fast one.
	 */
	private static final AtomicLong ROWS = new AtomicLong();

	/** Populate the one row the fixture's {@code @BeforeEach} inserts. */
	private static void seed(SessionFactory sf) {
		try ( Session session = sf.openSession() ) {
			final Transaction tx = session.beginTransaction();
			final SimpleEntity simple = new SimpleEntity();
			simple.setId( 1 );
			simple.setGender( Gender.FEMALE );
			simple.setName( "Fab" );
			simple.setGender2( Gender.MALE );
			simple.setComponent( new Component( "a1", "a2" ) );
			session.persist( simple );
			final OtherEntity other = new OtherEntity();
			other.setId( 2 );
			other.setName( "Bar" );
			session.persist( other );
			tx.commit();
		}
	}

	private static Runnable unitFor(String mode, SessionFactory sf) {
		switch ( mode ) {
			case "txn":
				return () -> {
					try ( Session session = sf.openSession() ) {
						final Transaction tx = session.beginTransaction();
						tx.commit();
					}
				};
			case "session":
				return () -> {
					try ( Session session = sf.openSession() ) {
						// Touch the session so an implementation that opens
						// lazily still does the work.
						session.isOpen();
					}
				};
			case "jdbc":
				return () -> {
					try ( Connection c = DriverManager.getConnection( JDBC_URL, "sa", "" ) ) {
						c.setAutoCommit( false );
						c.commit();
					}
					catch (java.sql.SQLException e) {
						throw new RuntimeException( e );
					}
				};
			case "parse":
				return () -> {
					try ( Session session = sf.openSession() ) {
						final Transaction tx = session.beginTransaction();
						session.createQuery( Q1, Component.class );
						session.createQuery( Q2, Component.class );
						session.createQuery( Q3, SimpleEntity.class );
						tx.commit();
					}
				};
			case "exec":
				return () -> {
					try ( Session session = sf.openSession() ) {
						final Transaction tx = session.beginTransaction();
						final Query<Component> q =
								session.createQuery( Q1, Component.class );
						ROWS.addAndGet( q.setParameter( "param", "a1" ).list().size() );
						tx.commit();
					}
				};
			case "full":
				return () -> {
					try ( Session session = sf.openSession() ) {
						final Transaction tx = session.beginTransaction();
						final Query<Component> q1 =
								session.createQuery( Q1, Component.class );
						ROWS.addAndGet( q1.setParameter( "param", "a1" ).list().size() );
						final Query<Component> q2 =
								session.createQuery( Q2, Component.class );
						ROWS.addAndGet( q2.setParameter( "param", "b1" ).list().size() );
						final Query<SimpleEntity> q3 =
								session.createQuery( Q3, SimpleEntity.class );
						ROWS.addAndGet( q3.setParameter( "param", "a1" ).list().size() );
						tx.commit();
					}
				};
			default:
				throw new IllegalArgumentException(
						"unknown mode '" + mode
								+ "' (txn|session|jdbc|parse|exec|full)" );
		}
	}

	private static final String Q1 =
			"select e.component from SimpleEntity e where e.component.attribute1 = :param";
	private static final String Q2 =
			"select e.component from SimpleEntity e where e.component.attribute1 = :param";
	private static final String Q3 =
			"select e from SimpleEntity e where e.component.attribute1 = :param";

	/**
	 * The real test's structure: a fresh task list per fork, submitted with
	 * {@code invokeAll}, which parks the submitting thread until the fork
	 * drains. Reproducing that — rather than one flat pool submission — matters,
	 * because the park/unpark of the main thread once per fork is part of the
	 * workload.
	 *
	 * <p>Unlike the real test, every returned {@code Future} is inspected.
	 * {@code invokeAll} captures a task's exception in its Future instead of
	 * propagating it, and the fixture drops the returned list on the floor — so
	 * a run in which <em>every single unit threw immediately</em> completes in
	 * milliseconds and reports success. A throughput probe with that hole
	 * measures the cost of throwing. The first failure is rethrown here, and
	 * the caller turns it into a non-zero exit.
	 */
	private static void runForks(int forks, int iterations, int threads, Runnable unit)
			throws Exception {
		final ExecutorService executor = Executors.newFixedThreadPool( threads );
		try {
			for ( int f = 0; f < forks; f++ ) {
				final List<Callable<String>> tasks = new ArrayList<>( iterations );
				for ( int i = 0; i < iterations; i++ ) {
					tasks.add( () -> {
						unit.run();
						return null;
					} );
				}
				for ( Future<String> future : executor.invokeAll( tasks ) ) {
					try {
						future.get();
					}
					catch (ExecutionException e) {
						throw new IllegalStateException(
								"a unit of work threw — this run measured nothing", e.getCause() );
					}
				}
			}
		}
		finally {
			executor.shutdown();
			executor.awaitTermination( 60, TimeUnit.SECONDS );
		}
	}

	/**
	 * The fixture's entity shape, attribute for attribute — including both
	 * enum mappings and the nested embeddable. The extrapolation to
	 * {@code PROJECTED_TEST_MS} is only meaningful if the row being hydrated
	 * costs what the real one costs, and a shape short of two enum columns and
	 * a nested component reads ~4x cheaper per unit than the real test body.
	 */
	@Entity(name = "SimpleEntity")
	@Table(name = "mapping_simple_entity")
	public static class SimpleEntity {
		private Integer id;
		private String name;
		private Gender gender;
		private Gender gender2;
		private Component component;

		@Id
		public Integer getId() {
			return id;
		}

		public void setId(Integer id) {
			this.id = id;
		}

		public String getName() {
			return name;
		}

		public void setName(String name) {
			this.name = name;
		}

		@Enumerated
		public Gender getGender() {
			return gender;
		}

		public void setGender(Gender gender) {
			this.gender = gender;
		}

		@Enumerated(EnumType.STRING)
		public Gender getGender2() {
			return gender2;
		}

		public void setGender2(Gender gender2) {
			this.gender2 = gender2;
		}

		@Embedded
		public Component getComponent() {
			return component;
		}

		public void setComponent(Component component) {
			this.component = component;
		}
	}

	public enum Gender {
		MALE,
		FEMALE
	}

	@Entity(name = "OtherEntity")
	@Table(name = "mapping_other_entity")
	public static class OtherEntity {
		private Integer id;
		private String name;

		@Id
		public Integer getId() {
			return id;
		}

		public void setId(Integer id) {
			this.id = id;
		}

		public String getName() {
			return name;
		}

		public void setName(String name) {
			this.name = name;
		}
	}

	@Embeddable
	public static class Component {
		private String attribute1;
		private String attribute2;
		private SubComponent subComponent;

		public Component() {
		}

		public Component(String attribute1, String attribute2) {
			this.attribute1 = attribute1;
			this.attribute2 = attribute2;
		}

		public String getAttribute1() {
			return attribute1;
		}

		public void setAttribute1(String attribute1) {
			this.attribute1 = attribute1;
		}

		public String getAttribute2() {
			return attribute2;
		}

		public void setAttribute2(String attribute2) {
			this.attribute2 = attribute2;
		}

		@Embedded
		public SubComponent getSubComponent() {
			return subComponent;
		}

		public void setSubComponent(SubComponent subComponent) {
			this.subComponent = subComponent;
		}
	}

	@Embeddable
	public static class SubComponent {
		private String subAttribute1;
		private String subAttribute2;

		public SubComponent() {
		}

		public SubComponent(String subAttribute1, String subAttribute2) {
			this.subAttribute1 = subAttribute1;
			this.subAttribute2 = subAttribute2;
		}

		public String getSubAttribute1() {
			return subAttribute1;
		}

		public void setSubAttribute1(String subAttribute1) {
			this.subAttribute1 = subAttribute1;
		}

		public String getSubAttribute2() {
			return subAttribute2;
		}

		public void setSubAttribute2(String subAttribute2) {
			this.subAttribute2 = subAttribute2;
		}
	}
}
