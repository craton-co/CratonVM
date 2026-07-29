import org.springframework.core.ResolvableType;
import org.springframework.util.ConcurrentReferenceHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Reproduces the ResolvableType static-cache lookup shape with a map typed
 * <Object,Object> so a wrong lookup result is REPORTED (with its real class)
 * instead of being hidden behind javac's checkcast. Counts every get() whose
 * result is not the object that was put.
 */
public class CrhmProbe {

    static final Class<?>[] CS = {
        String.class, Integer.class, Long.class, Double.class,
        List.class, Map.class, Set.class, Object.class, Number.class,
        CharSequence.class, Comparable.class, Runnable.class,
    };

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2000;

        ConcurrentReferenceHashMap<Object, Object> cache = new ConcurrentReferenceHashMap<>(256);
        ResolvableType[] keys = new ResolvableType[CS.length];
        for (int i = 0; i < CS.length; i++) {
            keys[i] = ResolvableType.forClassWithGenerics(List.class, CS[i]);
            cache.put(keys[i], keys[i]);
        }
        // Fresh, structurally-equal probe keys (equals()/hashCode() must match).
        ResolvableType[] probes = new ResolvableType[CS.length];
        for (int i = 0; i < CS.length; i++) {
            probes[i] = ResolvableType.forClassWithGenerics(List.class, CS[i]);
        }

        long bad = 0, nulls = 0;
        int reported = 0;
        for (int it = 0; it < iters; it++) {
            for (int i = 0; i < keys.length; i++) {
                Object got = cache.get(probes[i]);
                if (got == null) {
                    nulls++;
                } else if (got != keys[i]) {
                    bad++;
                    if (reported++ < 5) {
                        System.out.println("WRONG at iter=" + it + " i=" + i
                                + " gotClass=" + got.getClass().getName()
                                + " expectedClass=" + keys[i].getClass().getName()
                                + " gotToString=" + safe(got));
                    }
                }
            }
        }
        System.out.println("ITERS=" + iters);
        System.out.println("NULL_COUNT=" + nulls);
        System.out.println("WRONG_COUNT=" + bad);
        System.out.println("MISMATCH_COUNT=" + (bad + nulls));
    }

    static String safe(Object o) {
        try {
            return String.valueOf(o);
        } catch (Throwable t) {
            return "<toString threw " + t + ">";
        }
    }
}
