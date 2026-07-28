import java.sql.*;

/**
 * Plain-JDBC (no Hibernate) probe for the OVER(PARTITION BY ...) window-function
 * family of known-issue docs:
 *   - orderedsetaggregate-window-partition-value-staleness-20260727
 *   - windowfunction-partition-rowid-lookup-miss-shift-20260727
 *   - bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727
 *
 * Fixture mirrors Hibernate's EntityOfBasics rows used by
 * CriteriaOrderedSetAggregateTest / WindowFunctionTest:
 *   id=1 the_int=5  the_string='5'
 *   id=2 the_int=6  the_string='6'
 *   id=3 the_int=7  the_string='7'
 *   id=4 the_int=13 the_string='13'
 *   id=5 the_int=5  the_string='5'
 *
 * Each query prints its extracted per-row values as a single line so a CratonVM
 * run can be diffed byte-for-byte against a real HotSpot run of the same class.
 */
public class H2OsaWindowProbe {

	public static void main(String[] args) throws Exception {
		Class.forName( "org.h2.Driver" );
		try ( Connection c = DriverManager.getConnection(
				"jdbc:h2:mem:osaprobe;DB_CLOSE_DELAY=-1", "sa", "" ) ) {
			try ( Statement s = c.createStatement() ) {
				s.execute( "create table EntityOfBasics ("
						+ "id integer not null, the_int integer, the_string varchar(255), "
						+ "primary key (id))" );
			}
			int[] ids = { 1, 2, 3, 4, 5 };
			int[] ints = { 5, 6, 7, 13, 5 };
			try ( PreparedStatement ps = c.prepareStatement(
					"insert into EntityOfBasics (id, the_int, the_string) values (?,?,?)" ) ) {
				for ( int i = 0; i < ids.length; i++ ) {
					ps.setInt( 1, ids[i] );
					ps.setInt( 2, ints[i] );
					ps.setString( 3, String.valueOf( ints[i] ) );
					ps.executeUpdate();
				}
			}

			// --- ordered-set aggregates used as window functions (doc 1) ---
			scan( c, "OSA.percentile_disc.window",
					"select percentile_disc(0.5) within group (order by eob1_0.the_int)"
							+ " over(partition by eob1_0.the_int) from EntityOfBasics eob1_0 order by 1" );
			scan( c, "OSA.listagg.filter.window",
					"select listagg(eob1_0.the_string, ',') within group (order by eob1_0.id desc)"
							+ " filter (where eob1_0.the_int<10) over(partition by eob1_0.the_int)"
							+ " from EntityOfBasics eob1_0" );
			// plain (non-window) ordered-set aggregate control -- expected to pass today
			scan( c, "OSA.percentile_disc.plain",
					"select percentile_disc(0.5) within group (order by eob1_0.the_int)"
							+ " from EntityOfBasics eob1_0" );
			scan( c, "OSA.listagg.plain",
					"select listagg(eob1_0.the_string, ',') within group (order by eob1_0.id desc)"
							+ " from EntityOfBasics eob1_0" );

			// --- rank/row_number/dense_rank as window functions (doc 2) ---
			scan( c, "WF.rank.partition",
					"select rank() over(partition by eob1_0.the_int order by eob1_0.id)"
							+ " from EntityOfBasics eob1_0 order by 1" );
			scan( c, "WF.rownumber.noorder",
					"select row_number() over() from EntityOfBasics eob1_0" );
			scan( c, "WF.rownumber.partition",
					"select eob1_0.id, row_number() over(partition by eob1_0.the_int"
							+ " order by eob1_0.id) from EntityOfBasics eob1_0 order by eob1_0.id" );
			scan( c, "WF.denserank.partition",
					"select eob1_0.id, dense_rank() over(partition by eob1_0.the_int"
							+ " order by eob1_0.id) from EntityOfBasics eob1_0 order by eob1_0.id" );
			scan( c, "WF.sum.filter.window",
					"select sum(eob1_0.the_int) filter (where eob1_0.the_int > 5)"
							+ " over(order by eob1_0.the_int) from EntityOfBasics eob1_0"
							+ " order by eob1_0.the_int" );
			scan( c, "WF.frame",
					"select sum(eob1_0.the_int) over(order by eob1_0.id"
							+ " rows between 1 preceding and 1 following) from EntityOfBasics eob1_0"
							+ " order by eob1_0.id" );
			scan( c, "WF.lag.partition",
					"select eob1_0.id, lag(eob1_0.the_int) over(partition by eob1_0.the_int"
							+ " order by eob1_0.id) from EntityOfBasics eob1_0 order by eob1_0.id" );
			scan( c, "WF.avg.partition",
					"select eob1_0.id, avg(eob1_0.the_int) over(partition by eob1_0.the_int)"
							+ " from EntityOfBasics eob1_0 order by eob1_0.id" );
			scan( c, "WF.count.partition",
					"select eob1_0.id, count(*) over(partition by eob1_0.the_int)"
							+ " from EntityOfBasics eob1_0 order by eob1_0.id" );
			scan( c, "WF.rownumber.groupby",
					"select eob1_0.the_int, row_number() over(order by eob1_0.the_int)"
							+ " from EntityOfBasics eob1_0 group by eob1_0.the_int order by 1" );

			// --- bulkid shape (doc 3): row_number() over() INSERT ... SELECT ---
			try ( Statement s = c.createStatement() ) {
				s.execute( "create table Scratch (id integer not null, rn_ integer not null,"
						+ " primary key (id))" );
				int n = s.executeUpdate( "insert into Scratch (id, rn_)"
						+ " select (eob1_0.id+20), row_number() over() from EntityOfBasics eob1_0" );
				System.out.println( String.format( "%-28s rows=%d", "BULK.insertselect.count", n ) );
			}
			scan( c, "BULK.insertselect.readback",
					"select id, rn_ from Scratch order by id" );
		}
	}

	static void scan(Connection c, String label, String sql) {
		StringBuilder sb = new StringBuilder();
		int n = 0;
		try ( Statement s = c.createStatement(); ResultSet rs = s.executeQuery( sql ) ) {
			int cols = rs.getMetaData().getColumnCount();
			while ( rs.next() ) {
				if ( n > 0 ) {
					sb.append( ' ' );
				}
				for ( int i = 1; i <= cols; i++ ) {
					sb.append( rs.getObject( i ) );
					if ( i != cols ) {
						sb.append( '/' );
					}
				}
				n++;
			}
		}
		catch (SQLException e) {
			sb.append( "EX: " ).append( e.getMessage() );
		}
		System.out.println( String.format( "%-28s n=%-3d %s", label, n, sb ) );
	}
}
