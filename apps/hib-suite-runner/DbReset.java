import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Statement;
import java.util.ArrayList;
import java.util.List;

/**
 * Drop every table (and Postgres sequence) in a worker database, so the next
 * test class starts against an empty schema.
 *
 * WHY THIS EXISTS.
 *
 * `run-hib.sh` gives each shard one worker database and never resets it. That
 * is fine as long as every class removes its own schema, which every class that
 * EXITS NORMALLY does — measured: `hql.ASTParserLoadingTest` under CratonVM
 * finishes with 21 test failures and still leaves `tables left: 0`.
 *
 * A class that is KILLED does not. `run-hib.sh` kills a class at `--timeout`
 * (300 s in the MySQL runs) and records it `HANG`; the JVM never reaches the
 * test framework's schema drop, and its tables stay. Reproduced exactly as the
 * harness does it:
 *
 *   timeout 300 cratonvm ... CratonRunner org.hibernate.orm.test.hql.ASTParserLoadingTest
 *   -> rc=124, and 54 tables left behind
 *      (Animal, Human, Zoo, Customer, Product, User, LineItem, ...)
 *
 * Because the database is never reset, that debris outlives the shard AND the
 * run. Dozens of unrelated Hibernate test classes declare their own `Person`,
 * `Product`, `Animal` and `User` entities with different column sets, so every
 * later class whose entity names collide fails on a schema it did not create:
 *
 *   CriteriaMutationQueryTableTest on the poisoned database  0/2
 *       Table 'Animal' already exists / Unknown column 'age'
 *   the same class on a fresh database                       2/2 PASS
 *
 * On the 2026-08-24 G1 run that is 45 failures after the leaker on its shard
 * against 17 before it. See
 * `fixed-suite-bugs/hibernate/mysql-cross-class-stale-schema-shared-worker-db-20260822.md`.
 *
 * RUN THIS ON THE STOCK JDK, not on the VM under test. The harness's own
 * hygiene must not depend on the binary whose defects it is measuring — if the
 * VM under test is what got killed, it is also the last thing that should be
 * trusted to clean up after it.
 *
 * usage: DbReset <jdbc-url> <user> <password> [--verify-only]
 *
 * Exit 0 on success (or when there was nothing to drop), 2 on a usage error,
 * 1 if tables survived the drop. `--verify-only` reports what is there and
 * drops nothing, which is how a leak on a CLEAN exit would still be visible
 * rather than silently papered over.
 */
public final class DbReset {
    private DbReset() {}

    public static void main(String[] args) throws Exception {
        if (args.length < 3) {
            System.err.println("usage: DbReset <jdbc-url> <user> <password> [--verify-only]");
            System.exit(2);
        }
        String url = args[0];
        String user = args[1];
        String password = args[2];
        boolean verifyOnly = args.length > 3 && "--verify-only".equals(args[3]);
        boolean postgres = url.startsWith("jdbc:postgresql:");

        try (Connection c = DriverManager.getConnection(url, user, password)) {
            List<String> tables = tables(c, postgres);
            if (tables.isEmpty()) {
                System.out.println("@@DBRESET clean tables=0");
                return;
            }
            if (verifyOnly) {
                System.out.println("@@DBRESET dirty tables=" + tables.size() + " " + tables);
                return;
            }
            drop(c, tables, postgres);

            List<String> left = tables(c, postgres);
            if (left.isEmpty()) {
                System.out.println("@@DBRESET reset tables_dropped=" + tables.size());
            } else {
                // Reported rather than thrown: a shard should keep running and
                // say so, not die on the cleanup step. The count is what makes
                // a partial reset visible in the log instead of showing up
                // later as somebody else's schema error.
                System.out.println("@@DBRESET partial tables_dropped="
                        + (tables.size() - left.size()) + " tables_left=" + left.size()
                        + " " + left);
                System.exit(1);
            }
        }
    }

    /** Every base table and view in the connection's own schema. */
    private static List<String> tables(Connection c, boolean postgres) throws SQLException {
        String sql = postgres
                ? "SELECT tablename FROM pg_tables WHERE schemaname = current_schema()"
                : "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()";
        List<String> out = new ArrayList<>();
        try (Statement s = c.createStatement(); ResultSet rs = s.executeQuery(sql)) {
            while (rs.next()) {
                out.add(rs.getString(1));
            }
        }
        return out;
    }

    private static void drop(Connection c, List<String> tables, boolean postgres)
            throws SQLException {
        try (Statement s = c.createStatement()) {
            // Hibernate's schemas are full of foreign keys and the drop order is
            // not knowable from the table list alone. Disabling the constraint
            // check (MySQL) / cascading (Postgres) is what makes one pass enough.
            if (!postgres) {
                s.execute("SET FOREIGN_KEY_CHECKS = 0");
            }
            for (String t : tables) {
                try {
                    s.execute("DROP TABLE IF EXISTS " + quote(t, postgres)
                            + (postgres ? " CASCADE" : ""));
                } catch (SQLException e) {
                    // A view listed as a table, or something another connection
                    // is holding. Keep going: the verify pass below is what
                    // decides whether the reset succeeded, not this loop.
                    try {
                        s.execute("DROP VIEW IF EXISTS " + quote(t, postgres)
                                + (postgres ? " CASCADE" : ""));
                    } catch (SQLException ignored) {
                        // reported by the caller's verify pass
                    }
                }
            }
            if (postgres) {
                // Postgres sequences are separate objects; MySQL's are tables and
                // were already covered above.
                List<String> seqs = new ArrayList<>();
                try (ResultSet rs = s.executeQuery(
                        "SELECT sequencename FROM pg_sequences WHERE schemaname = current_schema()")) {
                    while (rs.next()) {
                        seqs.add(rs.getString(1));
                    }
                }
                for (String q : seqs) {
                    try {
                        s.execute("DROP SEQUENCE IF EXISTS " + quote(q, true) + " CASCADE");
                    } catch (SQLException ignored) {
                        // as above
                    }
                }
            } else {
                s.execute("SET FOREIGN_KEY_CHECKS = 1");
            }
        }
    }

    /**
     * Quote an identifier the way its own server does.
     *
     * Not cosmetic: Hibernate's test schemas contain `one`, `many`, `image` and
     * `title`, and an unquoted `DROP TABLE one` is a syntax error on MySQL.
     */
    private static String quote(String identifier, boolean postgres) {
        char q = postgres ? '"' : '`';
        return q + identifier.replace(String.valueOf(q), String.valueOf(q) + q) + q;
    }
}
