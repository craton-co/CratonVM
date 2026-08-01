import java.util.ArrayList;
import java.util.List;

import org.hibernate.SessionFactory;
import org.hibernate.cfg.Configuration;

/**
 * Repeated {@code SessionFactory} bootstrap, for defects that surface once per
 * build rather than once per call.
 *
 * <p>The witness for the {@code Type.getTypeName()} wrong-receiver dispatch is
 * `ASTParserLoadingTest`, which builds ~32 session factories over ten minutes and
 * — in the runs that show the defect at all — logs
 * {@code NoSuchMethodError: java/lang/Integer.getTypeName()} once per build, from
 * about 2.5 minutes in through to the end. Two things about that shape make the
 * test class a poor probe: most of its ten minutes is spent on things unrelated to
 * bootstrap, and the defect only appears in some runs, so a single green run
 * proves nothing.
 *
 * <p>This does nothing but bootstrap, so the interesting event per unit time is
 * maximised. {@code JavaTypeRegistry.addBaselineDescriptor} — the reported caller —
 * runs a few dozen times per build, once for each baseline
 * {@code JavaType}, and the failure names {@code Integer} every time.
 *
 * <p>The probe cannot assert on the defect directly: the VM recovers from it and
 * Hibernate never sees an exception, so it is visible only as a VM-level warning
 * on stderr. Drive it with the runner and grep, e.g.
 *
 * <pre>
 * cratonvm ... SessionFactoryChurnProbe 40 2&gt;&amp;1 | grep -c getTypeName
 * </pre>
 *
 * <p>usage: {@code SessionFactoryChurnProbe [builds]} — prints
 * {@code @@SFCHURN builds=N ok=M}.
 */
public final class SessionFactoryChurnProbe {

	public static void main(String[] args) {
		final int builds = args.length > 0 ? Integer.parseInt( args[0] ) : 20;

		final List<String> failures = new ArrayList<>();
		int ok = 0;

		for ( int i = 0; i < builds; i++ ) {
			final Configuration cfg = new Configuration();
			cfg.addResource( "org/hibernate/orm/test/hql/Animal.hbm.xml" );
			cfg.setProperty( "hibernate.dialect", "org.hibernate.dialect.H2Dialect" );
			cfg.setProperty( "hibernate.connection.driver_class", "org.h2.Driver" );
			// A distinct in-memory database per build: a shared one would make the
			// later builds cheaper than the first in a way real per-test bootstraps
			// are not.
			cfg.setProperty( "hibernate.connection.url",
					"jdbc:h2:mem:sfchurn" + i + ";DB_CLOSE_DELAY=-1" );
			cfg.setProperty( "hibernate.connection.username", "sa" );
			cfg.setProperty( "hibernate.connection.password", "" );
			cfg.setProperty( "hibernate.hbm2ddl.auto", "create-drop" );
			cfg.setProperty( "hibernate.show_sql", "false" );
			cfg.setProperty( "hibernate.generate_statistics", "true" );
			cfg.setProperty( "hibernate.cache.use_query_cache", "true" );

			try ( SessionFactory sf = cfg.buildSessionFactory() ) {
				// Touch the type registry the reported caller populates, so a
				// build that silently produced a broken registry is visible here
				// and not only in the VM's own log.
				final Object intType =
						((org.hibernate.engine.spi.SessionFactoryImplementor) sf)
								.getTypeConfiguration()
								.getJavaTypeRegistry()
								.getDescriptor( Integer.class );
				if ( intType == null ) {
					failures.add( "build=" + i + " JavaTypeRegistry lost the Integer descriptor" );
				}
				else {
					ok++;
				}
			}
			catch (RuntimeException | Error e) {
				failures.add( "build=" + i + " " + e.getClass().getName() + ": " + e.getMessage() );
			}
		}

		for ( String failure : failures ) {
			System.out.println( "SFCHURN " + failure );
		}
		System.out.println( "@@SFCHURN builds=" + builds + " ok=" + ok
				+ " failures=" + failures.size() );
		if ( !failures.isEmpty() ) {
			System.exit( 1 );
		}
	}
}
