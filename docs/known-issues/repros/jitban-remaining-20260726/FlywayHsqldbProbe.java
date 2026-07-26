import org.hsqldb.jdbc.JDBCDataSource;

import java.sql.Connection;
import java.sql.ResultSet;
import java.sql.Statement;

// SPB-FLYWAY-HSQLDB.1 repro: drives HSQLDB DDL/DML directly against an
// in-memory database repeatedly. The original ban's comment describes a
// SIGSEGV hitting org/hsqldb/'s "dense add/update path" (HSQLDB's internal
// hash/index structures during DDL/DML) after JIT compilation, originally
// found via a Flyway+HSQLDB integration. Flyway 12.4.0 (the only version
// available on this host) dropped built-in HSQLDB support (no
// flyway-database-hsqldb plugin present either), so this probe exercises
// org/hsqldb/'s own dense add/update path directly via JDBC instead -- same
// banned package, without the Flyway/CGLIB wrapping layer. Loop many
// independent databases/schemas to cross JIT invocation thresholds on
// hsqldb's hot internal methods.
public class FlywayHsqldbProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        int failures = 0;

        for (int i = 0; i < iterations; i++) {
            String dbName = "probe_db_" + i;
            String url = "jdbc:hsqldb:mem:" + dbName + ";shutdown=true";

            JDBCDataSource ds = new JDBCDataSource();
            ds.setUrl(url);
            ds.setUser("SA");
            ds.setPassword("");

            try (Connection conn = ds.getConnection()) {
                try (Statement ddl = conn.createStatement()) {
                    ddl.executeUpdate("CREATE TABLE widgets (id INT PRIMARY KEY, name VARCHAR(50), value INT)");
                    ddl.executeUpdate("CREATE INDEX idx_widgets_name ON widgets(name)");
                    ddl.executeUpdate("CREATE TABLE marker (id INT PRIMARY KEY)");
                }
                // Dense add/update path: many inserts/updates into the same
                // table within one connection, then verify row count and a
                // handful of values round-trip correctly.
                try (Statement st = conn.createStatement()) {
                    for (int j = 0; j < 200; j++) {
                        st.executeUpdate("INSERT INTO widgets(id, name, value) VALUES ("
                                + j + ", 'w" + j + "', " + (j * 7) + ")");
                    }
                    for (int j = 0; j < 200; j += 2) {
                        st.executeUpdate("UPDATE widgets SET value = value + 1 WHERE id = " + j);
                    }
                    try (ResultSet rs = st.executeQuery("SELECT COUNT(*), SUM(value) FROM widgets")) {
                        rs.next();
                        long count = rs.getLong(1);
                        long sum = rs.getLong(2);
                        if (count != 200) {
                            failures++;
                            if (failures <= 5) {
                                System.out.println("ROW-COUNT MISMATCH at i=" + i + " count=" + count);
                            }
                        }
                        long expectedSum = 0;
                        for (int j = 0; j < 200; j++) {
                            expectedSum += j * 7 + (j % 2 == 0 ? 1 : 0);
                        }
                        if (sum != expectedSum) {
                            failures++;
                            if (failures <= 5) {
                                System.out.println("SUM MISMATCH at i=" + i + " sum=" + sum
                                        + " expected=" + expectedSum);
                            }
                        }
                    }
                }
            }

            if (i % 20 == 0) {
                System.out.println("progress i=" + i);
                System.out.flush();
            }
        }
        System.out.println("DONE iterations=" + iterations + " failures=" + failures);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
