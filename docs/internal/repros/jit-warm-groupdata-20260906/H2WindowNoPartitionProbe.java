import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;

/**
 * Minimal, Hibernate-free reproduction of CriteriaWindowFunctionTest's
 * testNthValue / testCountAsWindowFunctionWithFilter symptom: both queries
 * are window functions with an empty OVER() / no PARTITION BY over a 5-row
 * table, and Hibernate/CratonVM reports resultList.size()==1 (collapsed to
 * a single aggregate row) instead of 5 (HotSpot).
 *
 * Usage: H2WindowNoPartitionProbe
 */
public class H2WindowNoPartitionProbe {
    public static void main(String[] args) throws Exception {
        String url = "jdbc:h2:mem:probe5;DB_CLOSE_DELAY=-1";
        Class.forName("org.h2.Driver");
        Connection c = DriverManager.getConnection(url, "sa", "");
        c.setAutoCommit(false);
        Statement st = c.createStatement();
        st.execute("CREATE TABLE EntityOfBasics (id integer not null primary key, the_int integer)");
        int[] vals = {5, 1, 7, 13, 6};
        PreparedStatement ins = c.prepareStatement("INSERT INTO EntityOfBasics (id, the_int) VALUES (?, ?)");
        for (int i = 0; i < vals.length; i++) {
            ins.setInt(1, i + 1);
            ins.setInt(2, vals[i]);
            ins.executeUpdate();
        }
        c.commit();

        System.out.println("--- nth_value, no partition ---");
        ResultSet rs1 = st.executeQuery(
            "select nth_value(eob1_0.the_int, 2) over(order by eob1_0.the_int desc rows between unbounded preceding and unbounded following) "
            + "from EntityOfBasics eob1_0");
        int n1 = 0;
        while (rs1.next()) {
            n1++;
            System.out.println("  row " + n1 + ": " + rs1.getObject(1));
        }
        System.out.println("  rowCount=" + n1 + " expected=5");

        System.out.println("--- count(...) filter (...) over(), no partition ---");
        PreparedStatement ps2 = c.prepareStatement(
            "select count(eob1_0.id) filter (where eob1_0.id>cast(? as integer)) over() from EntityOfBasics eob1_0");
        ps2.setInt(1, 0);
        ResultSet rs2 = ps2.executeQuery();
        int n2 = 0;
        while (rs2.next()) {
            n2++;
            System.out.println("  row " + n2 + ": " + rs2.getObject(1));
        }
        System.out.println("  rowCount=" + n2 + " expected=5");

        c.close();
    }
}
