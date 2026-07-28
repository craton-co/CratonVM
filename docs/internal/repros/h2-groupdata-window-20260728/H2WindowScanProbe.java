import java.sql.*;

/** Narrows the bulkid INSERT..SELECT defect: which SELECT shapes yield wrong per-row values. */
public class H2WindowScanProbe {

	static int N = 10;

	public static void main(String[] args) throws Exception {
		Class.forName( "org.h2.Driver" );
		try ( Connection c = DriverManager.getConnection( "jdbc:h2:mem:probe2;DB_CLOSE_DELAY=-1", "sa", "" ) ) {
			try ( Statement s = c.createStatement() ) {
				s.execute( "create table Doctor (id integer not null, grp integer not null, primary key (id))" );
			}
			try ( PreparedStatement ps = c.prepareStatement( "insert into Doctor (id,grp) values (?,?)" ) ) {
				for ( int i = 0; i < N; i++ ) {
					ps.setInt( 1, i + 1 );
					ps.setInt( 2, i % 3 );
					ps.executeUpdate();
				}
			}

			String[] sqls = {
					"select id from Doctor",
					"select (id+20) from Doctor",
					"select id, row_number() over() from Doctor",
					"select (id+20), row_number() over() from Doctor",
					"select (id+20), count(*) over() from Doctor",
					"select sum(id) over() from Doctor",
					"select id, sum(id) over() from Doctor",
					"select max(id) over() from Doctor",
					"select sum(id) from Doctor",
					"select count(*) from Doctor",
					"select grp, sum(id) from Doctor group by grp",
					"select id from Doctor group by id",
					"select (id+20) from Doctor group by id",
					"select distinct (id+20) from Doctor",
					"select id, lag(id) over(order by id) from Doctor",
			};
			for ( String sql : sqls ) {
				scan( c, sql );
			}
		}
	}

	static void scan(Connection c, String sql) {
		StringBuilder sb = new StringBuilder();
		int n = 0;
		try ( Statement s = c.createStatement(); ResultSet rs = s.executeQuery( sql ) ) {
			int cols = rs.getMetaData().getColumnCount();
			while ( rs.next() ) {
				for ( int i = 1; i <= cols; i++ ) {
					sb.append( rs.getObject( i ) );
					sb.append( i == cols ? ' ' : '/' );
				}
				n++;
			}
		}
		catch (SQLException e) {
			sb.append( "EX: " ).append( e.getMessage() );
		}
		System.out.println( String.format( "n=%-3d %-52s : %s", n, sql, sb ) );
	}
}
