import java.lang.reflect.Field;
import java.sql.Connection;
import java.sql.DriverManager;
import java.util.Map;
import java.util.TreeMap;

import org.h2.bnf.Bnf;
import org.h2.bnf.RuleHead;
import org.h2.bnf.Sentence;
import org.h2.bnf.context.DbContents;
import org.h2.bnf.context.DbContextRule;
import org.h2.bnf.context.DbProcedure;
import org.h2.bnf.context.DbSchema;

/** Narrows the TestBnf.testProcedures differential: is the missing completion a timeout? */
public class BnfProc {
    public static void main(String[] args) throws Exception {
        // nanoTime sanity
        long a = System.nanoTime();
        long t0 = System.currentTimeMillis();
        while (System.currentTimeMillis() - t0 < 50) { /* spin */ }
        long b = System.nanoTime();
        System.out.println("nanoTime delta over ~50ms wall = " + (b - a) + " ns");

        Connection conn = DriverManager.getConnection("jdbc:h2:mem:bnfprobe", "sa", "");
        conn.createStatement().execute("DROP ALIAS IF EXISTS CUSTOM_PRINT");
        conn.createStatement().execute(
                "CREATE ALIAS CUSTOM_PRINT AS $$ void print(String s) { System.out.println(s); } $$");
        conn.createStatement().execute("DROP TABLE IF EXISTS TABLE_WITH_STRING_FIELD");
        conn.createStatement().execute(
                "CREATE TABLE TABLE_WITH_STRING_FIELD (STRING_FIELD VARCHAR(50), INT_FIELD integer)");

        DbContents contents = new DbContents();
        contents.readContents("jdbc:h2:./test", conn);
        DbSchema def = contents.getDefaultSchema();
        System.out.print("procedures:");
        for (DbProcedure p : def.getProcedures()) {
            System.out.print(" " + p.getName());
        }
        System.out.println();

        Bnf bnf = Bnf.getInstance(null);
        bnf.updateTopic("column_name", new DbContextRule(contents, DbContextRule.COLUMN));
        bnf.updateTopic("user_defined_function_name", new DbContextRule(contents, DbContextRule.PROCEDURE));
        bnf.linkStatements();

        for (int i = 0; i < 5; i++) {
            long s = System.nanoTime();
            Map<String, String> tokens = bnf.getNextTokenList("SELECT CUSTOM_PR");
            long e = System.nanoTime();
            System.out.println("run " + i + " took " + ((e - s) / 1000000) + " ms -> "
                    + new TreeMap<>(tokens));
        }

        // Manual replication with the 100ms guard disabled, to prove/refute the timeout.
        System.out.println("--- guard disabled ---");
        Sentence sentence = new Sentence();
        sentence.setQuery("SELECT CUSTOM_PR");
        Field stopAt = Sentence.class.getDeclaredField("stopAtNs");
        stopAt.setAccessible(true);
        Field statementsF = Bnf.class.getDeclaredField("statements");
        statementsF.setAccessible(true);
        @SuppressWarnings("unchecked")
        java.util.ArrayList<RuleHead> statements = (java.util.ArrayList<RuleHead>) statementsF.get(bnf);
        long s = System.nanoTime();
        int visited = 0;
        try {
            for (RuleHead head : statements) {
                if (!head.getSection().startsWith("Commands")) {
                    continue;
                }
                sentence.start();
                stopAt.setLong(sentence, System.nanoTime() + 3600L * 1000 * 1000 * 1000);
                visited++;
                if (head.getRule().autoComplete(sentence)) {
                    break;
                }
            }
        } catch (IllegalStateException ex) {
            System.out.println("still threw ISE");
        }
        long e = System.nanoTime();
        System.out.println("no-guard walk: " + visited + " heads, " + ((e - s) / 1000000)
                + " ms -> " + new TreeMap<>(sentence.getNext()));
        conn.close();
    }
}
