import java.text.Collator;
import java.text.RuleBasedCollator;
import java.util.*;

/**
 * Can the REAL JDK collation machinery run on this VM? CratonVM overrides
 * java.text.Collator with a synthetic stub whose getInstance(Locale) discards
 * its argument, so the question is whether the stub is covering for a genuine
 * gap or is just switched-on code standing in front of a working path.
 *
 * Everything here reaches the real bytecode by a route the stub does not
 * intercept: an explicit RuleBasedCollator, the resource bundle the JDK's own
 * CollatorProvider reads, and the base rules it concatenates.
 */
public class RealCollatorProbe {
    static void step(String what, Runnable r) {
        try { r.run(); }
        catch (Throwable t) { System.out.println(what + " THREW " + t.getClass().getName() + ": " + t.getMessage()); }
    }

    public static void main(String[] a) {
        // 1. Can we build a RuleBasedCollator from an explicit rule string?
        step("1 explicit RuleBasedCollator", () -> {
            try {
                RuleBasedCollator c = new RuleBasedCollator("< a < b < c < I < i");
                System.out.println("1 explicit RuleBasedCollator ok; compare(I,i)=" + Integer.signum(c.compare("I", "i")));
            } catch (Exception e) { throw new RuntimeException(e); }
        });

        // 2. Is the JDK's base rule table reachable?
        step("2 CollationRules.DEFAULTRULES", () -> {
            try {
                Class<?> k = Class.forName("sun.util.locale.provider.CollationRules");
                java.lang.reflect.Field f = k.getDeclaredField("DEFAULTRULES");
                f.setAccessible(true);
                String s = (String) f.get(null);
                System.out.println("2 DEFAULTRULES ok, length=" + (s == null ? -1 : s.length()));
            } catch (Throwable e) { throw new RuntimeException(e); }
        });

        // 3. Is the Turkish tailoring bundle loadable, and what is in it?
        step("3 CollationData_tr bundle", () -> {
            try {
                ResourceBundle b = ResourceBundle.getBundle("sun.text.resources.ext.CollationData", new Locale("tr"));
                String rule = b.getString("Rule");
                System.out.println("3 CollationData_tr ok, Rule.length=" + rule.length()
                    + " head=" + rule.substring(0, Math.min(60, rule.length())).replace('\n', ' '));
            } catch (Throwable e) { throw new RuntimeException(e); }
        });

        // 4. Build the Turkish collator the way the JDK's provider does, and check the rule.
        step("4 assembled Turkish collator", () -> {
            try {
                Class<?> k = Class.forName("sun.util.locale.provider.CollationRules");
                java.lang.reflect.Field f = k.getDeclaredField("DEFAULTRULES");
                f.setAccessible(true);
                String base = (String) f.get(null);
                ResourceBundle b = ResourceBundle.getBundle("sun.text.resources.ext.CollationData", new Locale("tr"));
                RuleBasedCollator c = new RuleBasedCollator(base + b.getString("Rule"));
                c.setStrength(Collator.IDENTICAL);
                System.out.println("4 assembled tr: compare(I,i)=" + Integer.signum(c.compare("I", "i"))
                    + " compare(İ,I)=" + Integer.signum(c.compare("İ", "I"))
                    + " compare(ı,i)=" + Integer.signum(c.compare("ı", "i")));
                c.setStrength(Collator.PRIMARY);
                System.out.println("4 assembled tr PRIMARY: I==i? " + (c.compare("I", "i") == 0));
            } catch (Throwable e) { throw new RuntimeException(e); }
        });

        // 5. What class does Collator.getInstance actually return here?
        step("5 getInstance class", () -> {
            Collator c = Collator.getInstance(new Locale("tr"));
            System.out.println("5 Collator.getInstance(tr) -> " + c.getClass().getName());
        });
        System.out.println("REAL_COLLATOR_END");
    }
}
