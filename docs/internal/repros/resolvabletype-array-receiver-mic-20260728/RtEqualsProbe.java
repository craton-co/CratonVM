import org.springframework.core.ResolvableType;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Drives org.springframework.core.ResolvableType's static
 * ConcurrentReferenceHashMap cache, which calls ResolvableType.equals(Object)
 * and hashCode() on every probe, and re-checks the ResolvableType[] generics
 * arrays those lookups hand back. Reports a MISMATCH COUNT (0 == clean).
 */
public class RtEqualsProbe {

    static final Class<?>[] CS = {
        String.class, Integer.class, Long.class, Double.class,
        List.class, Map.class, Set.class, Object.class, Number.class,
        CharSequence.class, Comparable.class, Runnable.class,
    };

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int bad = 0;
        int cce = 0;
        int other = 0;
        for (int i = 0; i < iters; i++) {
            Class<?> g1 = CS[i % CS.length];
            Class<?> g2 = CS[(i / CS.length) % CS.length];
            try {
                ResolvableType t = ResolvableType.forClassWithGenerics(Map.class, g1, g2);
                ResolvableType[] gen = t.getGenerics();
                if (gen.length != 2) { bad++; continue; }
                if (gen[0].resolve() != g1) bad++;
                if (gen[1].resolve() != g2) bad++;
                if (t.resolve() != Map.class) bad++;

                // A second, structurally identical build must compare equal and
                // must come back from the cache as the same generics.
                ResolvableType t2 = ResolvableType.forClassWithGenerics(Map.class, g1, g2);
                if (!t.equals(t2)) bad++;
                if (t.hashCode() != t2.hashCode()) bad++;

                // ... and a structurally different one must NOT.
                ResolvableType t3 = ResolvableType.forClassWithGenerics(List.class, g1);
                if (t.equals(t3)) bad++;
                if (t3.getGenerics().length != 1) bad++;
                if (t3.getGenerics()[0].resolve() != g1) bad++;
                if (t.equals(null)) bad++;
                if (t.equals("not a ResolvableType")) bad++;
            } catch (ClassCastException e) {
                cce++;
                if (cce <= 3) {
                    System.out.println("CCE at i=" + i + ": " + e.getMessage());
                    e.printStackTrace(System.out);
                }
            } catch (Throwable e) {
                other++;
                if (other <= 3) {
                    System.out.println("OTHER at i=" + i + ": " + e);
                }
            }
        }
        System.out.println("ITERS=" + iters);
        System.out.println("CCE_COUNT=" + cce);
        System.out.println("OTHER_COUNT=" + other);
        System.out.println("MISMATCH_COUNT=" + bad);
    }
}
