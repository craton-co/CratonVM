import java.sql.*;

/**
 * Hibernate-free insert-rate probe.
 *
 * Splits the OracleInlineMutationStrategyIdTest fixture cost into "what H2
 * itself costs under CratonVM" versus "what Hibernate adds on top". The test's
 * @BeforeEach persists 2200 JOINED-inheritance entities, which Hibernate turns
 * into 4400 single-row INSERTs (a Person row plus a Doctor/Engineer row each)
 * inside one transaction. This runs exactly that JDBC traffic with no
 * Hibernate, no ORM and no logging.
 *
 * Usage: H2InsertRateProbe [entityCount]   (default 1100, the test's value)
 */
public class H2InsertRateProbe {

	public static void main(String[] args) throws Exception {
		final int n = args.length > 0 ? Integer.parseInt( args[0] ) : 1100;
		Class.forName( "org.h2.Driver" );
		try ( Connection c = DriverManager.getConnection(
				"jdbc:h2:mem:insrate;DB_CLOSE_DELAY=-1", "sa", "" ) ) {
			c.setAutoCommit( false );
			try ( Statement s = c.createStatement() ) {
				s.execute( "create table Person (id integer not null, name varchar(255),"
						+ " employed boolean not null, primary key (id))" );
				s.execute( "create table Doctor (id integer not null, primary key (id))" );
				s.execute( "create table Engineer (fellow boolean not null, id integer not null,"
						+ " primary key (id))" );
			}
			c.commit();

			// Round 1 warms H2's own caches; round 2 is the reported number.
			for ( int round = 1; round <= 2; round++ ) {
				try ( Statement s = c.createStatement() ) {
					s.execute( "delete from Doctor" );
					s.execute( "delete from Engineer" );
					s.execute( "delete from Person" );
				}
				c.commit();

				final long t0 = System.nanoTime();
				try ( PreparedStatement person = c.prepareStatement(
						"insert into Person (name,employed,id) values (?,?,?)" );
						PreparedStatement doctor = c.prepareStatement(
								"insert into Doctor (id) values (?)" );
						PreparedStatement engineer = c.prepareStatement(
								"insert into Engineer (fellow,id) values (?,?)" ) ) {
					for ( int i = 0; i < n; i++ ) {
						person.setString( 1, null );
						person.setBoolean( 2, ( i % 2 ) == 0 );
						person.setInt( 3, i + 1 );
						person.executeUpdate();
						doctor.setInt( 1, i + 1 );
						doctor.executeUpdate();
					}
					for ( int i = 0; i < n; i++ ) {
						person.setString( 1, null );
						person.setBoolean( 2, ( i % 2 ) == 0 );
						person.setInt( 3, i + 1 + n );
						person.executeUpdate();
						engineer.setBoolean( 1, ( i % 2 ) == 1 );
						engineer.setInt( 2, i + 1 + n );
						engineer.executeUpdate();
					}
				}
				c.commit();
				final long ms = ( System.nanoTime() - t0 ) / 1_000_000;
				final int stmts = n * 4;
				System.out.println( String.format(
						"round=%d entityCount=%d inserts=%d total=%d ms  per-insert=%.3f ms",
						round, n, stmts, ms, ms / (double) stmts ) );
			}

			// The bulk statements the test methods themselves run.
			timed( c, "bulk update", "update Person set name='John Doe' where employed=true" );
			timed( c, "bulk delete engineer", "delete from Engineer where fellow=true" );
			timed( c, "select count", "select count(*) from Person" );
		}
	}

	static void timed(Connection c, String label, String sql) throws SQLException {
		final long t0 = System.nanoTime();
		int rows;
		try ( Statement s = c.createStatement() ) {
			if ( sql.startsWith( "select" ) ) {
				try ( ResultSet rs = s.executeQuery( sql ) ) {
					rs.next();
					rows = rs.getInt( 1 );
				}
			}
			else {
				rows = s.executeUpdate( sql );
			}
		}
		c.commit();
		System.out.println( String.format( "%-22s rows=%-6d %d ms", label, rows,
				( System.nanoTime() - t0 ) / 1_000_000 ) );
	}
}
