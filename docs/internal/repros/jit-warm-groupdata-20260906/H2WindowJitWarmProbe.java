import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;

/**
 * Tests whether the CriteriaWindowFunctionTest#testNthValue /
 * #testCountAsWindowFunctionWithFilter symptom (resultList.size()==1
 * instead of 5, only observed when run as part of a larger JIT-enabled
 * suite, never in a single-shot probe or under --nojit) is a JIT
 * WARM-UP-dependent miscompile: repeats the SAME no-partition window query
 * many times in one JVM process (to force the H2 bytecode executing
 * Select.gatherGroup / SelectGroups / DataAnalysisOperation across the
 * groupData two-phase buffer to tier up), printing the row count of EVERY
 * iteration so a flip from 5 to 1 (or similar) at some iteration would be
 * visible directly.
 *
 * Usage: H2WindowJitWarmProbe <iterations>
 */
public class H2WindowJitWarmProbe {
    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        String url = "jdbc:h2:mem:probe6;DB_CLOSE_DELAY=-1";
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

        PreparedStatement ps = c.prepareStatement(
            "select nth_value(eob1_0.the_int, 2) over(order by eob1_0.the_int desc rows between unbounded preceding and unbounded following) "
            + "from EntityOfBasics eob1_0");

        int firstBadIter = -1;
        int badCount = 0;
        for (int iter = 0; iter < iters; iter++) {
            ResultSet rs = ps.executeQuery();
            int n = 0;
            Object last = null;
            while (rs.next()) {
                n++;
                last = rs.getObject(1);
            }
            rs.close();
            if (n != 5 || !Integer.valueOf(7).equals(last)) {
                badCount++;
                if (firstBadIter < 0) {
                    firstBadIter = iter;
                    System.out.println("FIRST BAD at iter=" + iter + " rowCount=" + n + " lastValue=" + last);
                }
            }
        }
        System.out.println("iters=" + iters + " badCount=" + badCount + " firstBadIter=" + firstBadIter);

        c.close();
    }
}
