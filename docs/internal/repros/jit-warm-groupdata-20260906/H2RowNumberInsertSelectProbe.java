import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;

/**
 * Replays the exact SQL shape Hibernate's TableBasedInsertHandler emits for
 * OracleInlineMutationStrategyIdTest.testInsertSelect (JOINED-inheritance
 * bulk HQL insert), at full 1100-row scale, without Hibernate:
 *
 *   create global temporary table HTE_Engineer(id bigint, name varchar,
 *     employed boolean, fellow boolean, rn_ bigint) transactional
 *   insert into HTE_Engineer (id, name, employed, fellow, rn_)
 *     select (d1_0.id+2200), 'John Doe', true, false, row_number() over()
 *     from Doctor d1_0
 *   insert into Person(name, employed, id)
 *     select hte_tmp.name, hte_tmp.employed, hte_tmp.id from HTE_Engineer hte_tmp
 *
 * Usage: H2RowNumberInsertSelectProbe <rows>
 */
public class H2RowNumberInsertSelectProbe {
    public static void main(String[] args) throws Exception {
        int rows = args.length > 0 ? Integer.parseInt(args[0]) : 1100;
        String url = "jdbc:h2:mem:probe3;DB_CLOSE_DELAY=-1";
        Class.forName("org.h2.Driver");
        Connection c = DriverManager.getConnection(url, "sa", "");
        c.setAutoCommit(false);
        Statement st = c.createStatement();
        st.execute("CREATE TABLE Doctor (id bigint not null primary key)");
        st.execute("CREATE TABLE Person (name varchar(255), employed boolean, id bigint not null primary key)");
        st.execute("CREATE GLOBAL TEMPORARY TABLE HTE_Engineer(id bigint, name varchar(255), employed boolean, fellow boolean, rn_ bigint) TRANSACTIONAL");

        PreparedStatement ins = c.prepareStatement("INSERT INTO Doctor (id) VALUES (?)");
        for (int i = 0; i < rows; i++) {
            ins.setLong(1, i + 1);
            ins.executeUpdate();
        }

        Statement st2 = c.createStatement();
        int hteInsertCount = st2.executeUpdate(
            "insert into HTE_Engineer (id, name, employed, fellow, rn_) "
            + "select (d1_0.id+" + (rows * 2) + "), 'John Doe', true, false, row_number() over() from Doctor d1_0");

        ResultSet rsHte = st2.executeQuery("select count(*), min(id), max(id), count(distinct rn_) from HTE_Engineer");
        rsHte.next();
        long hteCount = rsHte.getLong(1);
        long hteMinId = rsHte.getLong(2);
        long hteMaxId = rsHte.getLong(3);
        long hteDistinctRn = rsHte.getLong(4);

        int personInsertCount = st2.executeUpdate(
            "insert into Person(name, employed, id) select hte_tmp.name, hte_tmp.employed, hte_tmp.id from HTE_Engineer hte_tmp");

        ResultSet rsP = st2.executeQuery("select count(*) from Person");
        rsP.next();
        long personCount = rsP.getLong(1);

        c.commit();

        System.out.println("rows=" + rows
            + " hteInsertCount=" + hteInsertCount
            + " hteCount(actual)=" + hteCount
            + " hteMinId=" + hteMinId + " hteMaxId=" + hteMaxId
            + " hteDistinctRn=" + hteDistinctRn
            + " personInsertCount=" + personInsertCount
            + " personCount(actual)=" + personCount
            + " expectedMinId=" + (rows * 2 + 1) + " expectedMaxId=" + (rows * 2 + rows));

        c.close();
    }
}
