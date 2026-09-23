import java.text.Collator;
import java.util.Locale;

/** Is java.text.Collator.getInstance's per-locale cache effective? */
public class CollatorCache {
    public static void main(String[] args) throws Exception {
        long s = System.nanoTime();
        Collator first = Collator.getInstance();
        long e = System.nanoTime();
        System.out.println("first getInstance: " + ((e - s) / 1000000) + " ms, class=" + first.getClass().getName());
        for (int round = 0; round < 3; round++) {
            s = System.nanoTime();
            for (int i = 0; i < 200; i++) {
                Collator c = Collator.getInstance();
                c.setStrength(Collator.PRIMARY);
                if (!c.equals("SELECT", "select")) {
                    throw new AssertionError("collator equality broken");
                }
            }
            e = System.nanoTime();
            System.out.println("round " + round + ": 200 x getInstance+equals = " + ((e - s) / 1000000) + " ms");
        }
        s = System.nanoTime();
        for (int i = 0; i < 200; i++) {
            Collator.getInstance(Locale.US);
        }
        e = System.nanoTime();
        System.out.println("200 x getInstance(Locale.US) = " + ((e - s) / 1000000) + " ms");
    }
}
