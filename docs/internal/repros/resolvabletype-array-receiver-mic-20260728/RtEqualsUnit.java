import org.springframework.core.ResolvableType;
import org.springframework.util.ObjectUtils;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Isolates ResolvableType.equals(Object) and ObjectUtils.nullSafeEquals from
 * the ResolvableType cache: builds the instances ONCE up front, then hammers
 * the two comparison entry points and counts every answer that disagrees with
 * reference identity. Reports MISMATCH counts (0 == clean).
 */
public class RtEqualsUnit {

    static final Class<?>[] CS = {
        String.class, Integer.class, Long.class, Double.class,
        List.class, Map.class, Set.class, Object.class, Number.class,
        CharSequence.class, Comparable.class, Runnable.class,
    };

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2000;

        ResolvableType[] t = new ResolvableType[CS.length];
        for (int i = 0; i < CS.length; i++) {
            t[i] = ResolvableType.forClassWithGenerics(List.class, CS[i]);
        }
        // A second, structurally identical set (may or may not be identical
        // objects depending on the cache — equals() must say true either way).
        ResolvableType[] u = new ResolvableType[CS.length];
        for (int i = 0; i < CS.length; i++) {
            u[i] = ResolvableType.forClassWithGenerics(List.class, CS[i]);
        }

        long eqBad = 0, nseBad = 0, hashBad = 0, thrown = 0;
        int firstBadIter = -1;
        for (int it = 0; it < iters; it++) {
            for (int i = 0; i < t.length; i++) {
                for (int j = 0; j < t.length; j++) {
                    boolean expect = (i == j);
                    try {
                        if (t[i].equals(u[j]) != expect) {
                            eqBad++;
                            if (firstBadIter < 0) {
                                firstBadIter = it;
                                System.out.println("first equals() disagreement: iter=" + it
                                        + " i=" + i + " j=" + j + " expected=" + expect);
                            }
                        }
                        if (ObjectUtils.nullSafeEquals(t[i], u[j]) != expect) {
                            nseBad++;
                            if (firstBadIter < 0) {
                                firstBadIter = it;
                                System.out.println("first nullSafeEquals disagreement: iter=" + it
                                        + " i=" + i + " j=" + j + " expected=" + expect);
                            }
                        }
                        if (expect && t[i].hashCode() != u[j].hashCode()) {
                            hashBad++;
                        }
                    } catch (Throwable e) {
                        thrown++;
                        if (thrown <= 3) {
                            System.out.println("THROWN iter=" + it + " i=" + i + " j=" + j + ": " + e);
                            e.printStackTrace(System.out);
                        }
                    }
                }
            }
        }
        System.out.println("ITERS=" + iters);
        System.out.println("EQUALS_MISMATCH=" + eqBad);
        System.out.println("NULLSAFEEQUALS_MISMATCH=" + nseBad);
        System.out.println("HASH_MISMATCH=" + hashBad);
        System.out.println("THROWN_COUNT=" + thrown);
        System.out.println("MISMATCH_COUNT=" + (eqBad + nseBad + hashBad + thrown));
    }
}
