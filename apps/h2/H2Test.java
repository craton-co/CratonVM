import java.sql.*;

public class H2Test {
    public static void main(String[] args) throws Exception {
        Class.forName("org.h2.Driver");
        try (Connection c = DriverManager.getConnection("jdbc:h2:mem:probe;DB_CLOSE_DELAY=-1", "sa", "")) {
            try (Statement s = c.createStatement()) {
                s.execute("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(64))");
                s.execute("INSERT INTO t VALUES (1, 'one'), (2, 'two')");
            }
            try (Statement s = c.createStatement(); ResultSet rs = s.executeQuery("SELECT COUNT(*) FROM t")) {
                rs.next();
                int n = rs.getInt(1);
                System.out.println("count=" + n);
                if (n != 2) throw new AssertionError("count wrong");
            }
        }
        System.out.println("H2Test: PASS");
    }
}
