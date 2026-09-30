import java.lang.reflect.Field;
import java.sql.Connection;
import java.sql.DriverManager;
import java.util.ArrayList;
import java.util.TreeMap;

import org.h2.bnf.Bnf;
import org.h2.bnf.RuleHead;
import org.h2.bnf.Sentence;
import org.h2.bnf.context.DbContents;
import org.h2.bnf.context.DbContextRule;

/** Per-head cost of the cold TestBnf completion walk, against H2's 100 ms per-head budget. */
public class BnfHeads {
    public static void main(String[] args) throws Exception {
        Connection conn = DriverManager.getConnection("jdbc:h2:mem:bnfheads", "sa", "");
        conn.createStatement().execute(
                "CREATE ALIAS CUSTOM_PRINT AS $$ void print(String s) { System.out.println(s); } $$");
        conn.createStatement().execute(
                "CREATE TABLE TABLE_WITH_STRING_FIELD (STRING_FIELD VARCHAR(50), INT_FIELD integer)");
        DbContents contents = new DbContents();
        contents.readContents("jdbc:h2:./test", conn);
        Bnf bnf = Bnf.getInstance(null);
        bnf.updateTopic("column_name", new DbContextRule(contents, DbContextRule.COLUMN));
        bnf.updateTopic("user_defined_function_name", new DbContextRule(contents, DbContextRule.PROCEDURE));
        bnf.linkStatements();

        Field statementsF = Bnf.class.getDeclaredField("statements");
        statementsF.setAccessible(true);
        @SuppressWarnings("unchecked")
        ArrayList<RuleHead> statements = (ArrayList<RuleHead>) statementsF.get(bnf);

        Sentence sentence = new Sentence();
        sentence.setQuery("SELECT CUSTOM_PR");
        long walkStart = System.nanoTime();
        int n = 0;
        boolean tripped = false;
        for (RuleHead head : statements) {
            if (!head.getSection().startsWith("Commands")) {
                continue;
            }
            n++;
            sentence.start();
            long s = System.nanoTime();
            boolean done;
            try {
                done = head.getRule().autoComplete(sentence);
            } catch (IllegalStateException ex) {
                long e = System.nanoTime();
                System.out.println("head " + n + " '" + head.getTopic() + "' TRIPPED THE 100ms BUDGET after "
                        + ((e - s) / 1000000) + " ms");
                tripped = true;
                break;
            }
            long e = System.nanoTime();
            long ms = (e - s) / 1000000;
            if (ms >= 5) {
                System.out.println("head " + n + " '" + head.getTopic() + "' " + ms + " ms"
                        + (done ? " (matched, walk stops)" : ""));
            }
            if (done) {
                break;
            }
        }
        long walkEnd = System.nanoTime();
        System.out.println("walk: " + n + " heads, " + ((walkEnd - walkStart) / 1000000) + " ms, tripped="
                + tripped + " -> " + new TreeMap<>(sentence.getNext()));
        System.out.println("VERDICT " + (sentence.getNext().values().contains("INT") ? "PASS" : "FAIL"));
        conn.close();
    }
}
