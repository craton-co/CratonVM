import java.text.Collator;
import java.text.RuleBasedCollator;
import java.util.Locale;

/** Is the default locale (and therefore the collation table) the same on both VMs? */
public class LocaleCheck {
    public static void main(String[] args) {
        System.out.println("default locale      = " + Locale.getDefault());
        System.out.println("FORMAT              = " + Locale.getDefault(Locale.Category.FORMAT));
        System.out.println("user.language       = " + System.getProperty("user.language"));
        System.out.println("user.country        = " + System.getProperty("user.country"));
        System.out.println("java.locale.providers = " + System.getProperty("java.locale.providers"));
        Collator c = Collator.getInstance();
        System.out.println("collator class      = " + c.getClass().getName());
        if (c instanceof RuleBasedCollator) {
            String r = ((RuleBasedCollator) c).getRules();
            System.out.println("rules length        = " + r.length());
            System.out.println("rules hash          = " + r.hashCode());
        }
        System.out.println("decomposition       = " + c.getDecomposition());
    }
}
