import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;

/**
 * Replays the exact cross-test sequence that reproduces the
 * OracleInlineMutationStrategyIdTest testInsertSelect failure:
 *
 *   testInsert (runs first): single literal-values insert into the
 *     GLOBAL TEMPORARY TRANSACTIONAL HTE_Engineer table (rn_=1 explicit,
 *     no row_number()), read back via two plain SELECTs, then an explicit
 *     "delete from HTE_Engineer".
 *
 *   testInsertSelect (runs second, SAME connection/session): a 1100-row
 *     "insert into HTE_Engineer (...) select ..., row_number() over()
 *     from Doctor" into the SAME (already used-and-deleted) HTE_Engineer
 *     table.
 *
 * Isolates whether reusing a global temp table after a prior small
 * insert+delete cycle corrupts a later large row_number()-driven bulk
 * insert into that same table -- as opposed to a fresh table (which a
 * prior probe already showed works correctly at 1100-row scale).
 *
 * Usage: H2TempTableReuseProbe <rows>
 */
public class H2TempTableReuseProbe {
    public static void main(String[] args) throws Exception {
        int rows = args.length > 0 ? Integer.parseInt(args[0]) : 1100;
        String url = "jdbc:h2:mem:probe4;DB_CLOSE_DELAY=-1";
        Class.forName("org.h2.Driver");
        Connection c = DriverManager.getConnection(url, "sa", "");
        c.setAutoCommit(false);
        Statement st = c.createStatement();
        st.execute("CREATE TABLE Doctor (id bigint not null primary key)");
        st.execute("CREATE TABLE Person (name varchar(255), employed boolean, id bigint not null primary key)");
        st.execute("CREATE TABLE Engineer (id bigint not null primary key, fellow boolean)");
        st.execute("CREATE GLOBAL TEMPORARY TABLE HTE_Engineer(id bigint, name varchar(255), employed boolean, fellow boolean, rn_ bigint not null, primary key(rn_)) TRANSACTIONAL");

        // --- testInsert shape: one literal row, rn_=1 explicit ---
        st.executeUpdate("insert into HTE_Engineer (id, name, employed, fellow, rn_) values (0, 'John Doe', true, false, 1)");
        st.executeUpdate("insert into Person(name, employed, id) select hte_tmp.name, hte_tmp.employed, hte_tmp.id from HTE_Engineer hte_tmp");
        st.executeUpdate("insert into Engineer(id, fellow) select hte_tmp.id, hte_tmp.fellow from HTE_Engineer hte_tmp");
        st.executeUpdate("delete from HTE_Engineer");
        c.commit();

        // --- second "test": 1100-row Doctor table, row_number()-driven bulk insert into the SAME HTE_Engineer ---
        PreparedStatement ins = c.prepareStatement("INSERT INTO Doctor (id) VALUES (?)");
        for (int i = 0; i < rows; i++) {
            ins.setLong(1, i + 1);
            ins.executeUpdate();
        }

        int hteInsertCount = st.executeUpdate(
            "insert into HTE_Engineer (id, name, employed, fellow, rn_) "
            + "select (d1_0.id+" + (rows * 2) + "), 'John Doe', true, false, row_number() over() from Doctor d1_0");

        ResultSet rsHte = st.executeQuery("select count(*), min(id), max(id), count(distinct rn_) from HTE_Engineer");
        rsHte.next();
        long hteCount = rsHte.getLong(1);
        long hteMinId = rsHte.getLong(2);
        long hteMaxId = rsHte.getLong(3);
        long hteDistinctRn = rsHte.getLong(4);

        int personInsertCount = st.executeUpdate(
            "insert into Person(name, employed, id) select hte_tmp.name, hte_tmp.employed, hte_tmp.id from HTE_Engineer hte_tmp");

        c.commit();

        System.out.println("rows=" + rows
            + " hteInsertCount=" + hteInsertCount
            + " hteCount(actual)=" + hteCount
            + " hteMinId=" + hteMinId + " hteMaxId=" + hteMaxId
            + " hteDistinctRn=" + hteDistinctRn
            + " personInsertCount=" + personInsertCount
            + " expectedMinId=" + (rows * 2 + 1) + " expectedMaxId=" + (rows * 2 + rows));

        c.close();
    }
}
