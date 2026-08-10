import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.Map;

/**
 * Paired probe for identity-hash stability across garbage collection, and for
 * {@code HashMap} lookups keyed on objects that do not override
 * {@code hashCode}, diffed against the host JDK.
 *
 * {@code System.identityHashCode} must return the SAME value for the same
 * object for that object's whole life — including across a collection that
 * moves it. A collector that derives the hash from the address and does not
 * preserve it on copy breaks every hash container keyed by identity, and it
 * breaks them *silently*: `put` lands in one bucket, the object moves, and
 * `get` looks in another and returns null.
 *
 * Concretely, Spring Boot's `TomcatWebServer` parks the connectors it
 * temporarily removed in a `Map<Service, Connector[]>` and restores them from
 * it on `start()`. `Service` does not override `hashCode`. If that `get`
 * misses, the connectors are never restored, and `Tomcat.getConnector()` then
 * *fabricates* a replacement on port 8080 — which is exactly what CratonVM was
 * observed doing while HotSpot bound an ephemeral port.
 *
 * Every line prints a value, and the GC arms allocate hard enough to force
 * real collections rather than trusting `System.gc()`.
 */
public class IdentityHashAcrossGcProbe {

    /** No hashCode/equals override — identity semantics, like Tomcat's Service. */
    static final class Key {
        final int id;

        Key(int id) {
            this.id = id;
        }
    }

    static void churn(int mb) {
        // Allocate and drop, to make the collector actually run and move things.
        Object sink = null;
        for (int i = 0; i < mb * 32; i++) {
            byte[] b = new byte[32 * 1024];
            b[0] = (byte) i;
            if ((i & 1023) == 0) {
                sink = b;
            }
        }
        if (sink == null) {
            System.out.print("");
        }
    }

    public static void main(String[] args) {
        final int n = 200;
        Key[] keys = new Key[n];
        int[] hashBefore = new int[n];
        Map<Key, String> hash = new HashMap<>();
        Map<Key, String> ident = new IdentityHashMap<>();

        for (int i = 0; i < n; i++) {
            keys[i] = new Key(i);
            hashBefore[i] = System.identityHashCode(keys[i]);
            hash.put(keys[i], "v" + i);
            ident.put(keys[i], "v" + i);
        }

        System.out.println("initial hash.size()      = " + hash.size());
        System.out.println("initial ident.size()     = " + ident.size());

        churn(256);
        System.gc();
        churn(256);

        int hashChanged = 0;
        int hashMapMiss = 0;
        int identMapMiss = 0;
        int firstChanged = -1;
        for (int i = 0; i < n; i++) {
            if (System.identityHashCode(keys[i]) != hashBefore[i]) {
                hashChanged++;
                if (firstChanged < 0) {
                    firstChanged = i;
                }
            }
            if (!("v" + i).equals(hash.get(keys[i]))) {
                hashMapMiss++;
            }
            if (!("v" + i).equals(ident.get(keys[i]))) {
                identMapMiss++;
            }
        }

        System.out.println("expect 0: identityHashCode changed after GC = " + hashChanged
                + (firstChanged >= 0 ? " (first at index " + firstChanged + ")" : ""));
        System.out.println("expect 0: HashMap.get misses after GC       = " + hashMapMiss);
        System.out.println("expect 0: IdentityHashMap.get misses        = " + identMapMiss);
        System.out.println("expect 200: hash.size() after GC            = " + hash.size());

        // Second round, to catch a collector that only moves on a later cycle.
        churn(384);
        System.gc();
        int lateMiss = 0;
        for (int i = 0; i < n; i++) {
            if (!("v" + i).equals(hash.get(keys[i]))) {
                lateMiss++;
            }
        }
        System.out.println("expect 0: HashMap.get misses, 2nd round     = " + lateMiss);
    }
}
