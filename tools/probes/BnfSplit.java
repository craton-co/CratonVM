import java.lang.reflect.Field;
import java.sql.Connection;
import java.sql.DriverManager;
import java.text.Collator;
import java.util.ArrayList;

import org.h2.bnf.Bnf;
import org.h2.bnf.RuleHead;
import org.h2.bnf.Sentence;
import org.h2.bnf.context.DbContents;
import org.h2.bnf.context.DbContextRule;

/** Splits the cold SELECT head into collator bootstrap vs the rule walk itself. */
public class BnfSplit {
    public static void main(String[] args) throws Exception {
        boolean preheat = args.length > 0 && args[0].equals("preheat");
        Connection conn = DriverManager.getConnection("jdbc:h2:mem:bnfsplit", "sa", "");
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

        if (preheat) {
            long s = System.nanoTime();
            Collator c = Collator.getInstance();
            c.setStrength(Collator.PRIMARY);
            c.equals("SELECT", "select");
            System.out.println("collator bootstrap (outside the walk): " + ((System.nanoTime() - s) / 1000000) + " ms");
        }

        Field statementsF = Bnf.class.getDeclaredField("statements");
        statementsF.setAccessible(true);
        @SuppressWarnings("unchecked")
        ArrayList<RuleHead> statements = (ArrayList<RuleHead>) statementsF.get(bnf);
        RuleHead select = null;
        for (RuleHead h : statements) {
            if (h.getSection().startsWith("Commands")) { select = h; break; }
        }
        Sentence sentence = new Sentence();
        sentence.setQuery("SELECT CUSTOM_PR");
        Field stopAt = Sentence.class.getDeclaredField("stopAtNs");
        stopAt.setAccessible(true);
        sentence.start();
        stopAt.setLong(sentence, System.nanoTime() + 3600L * 1000_000_000L);
        long s = System.nanoTime();
        select.getRule().autoComplete(sentence);
        System.out.println("head 1 '" + select.getTopic() + "' (guard lifted): "
                + ((System.nanoTime() - s) / 1000000) + " ms");
        conn.close();
    }
}
