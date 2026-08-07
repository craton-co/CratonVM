import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.Map;

/**
 * Is {@code System.identityHashCode(o)} stable across a moving collection?
 *
 * {@code docs/known-issues/in-vm-javac-module-setup-fails-intermittently.md}
 * names this as the source of run-to-run nondeterminism it suspects: javac's
 * {@code Symbol}s do not override {@code hashCode()}, so every hash-ordered
 * walk in {@code Modules.setupAllModules} is ordered by identity hash. Varying
 * that order BETWEEN runs is normal and HotSpot does it too. Varying it WITHIN
 * a run — an object whose identity hash changes when the collector relocates it
 * — is not: it silently corrupts every hash container the object is already in.
 *
 * Prints one line per phase; the only thing that matters is that no line
 * reports a changed hash or a lost map entry.
 */
public class IdentityHashContainerStabilityProbe {

    static final int N = 4000;

    static void churn(int rounds) {
        // Allocation pressure, discarded — the point is to provoke collections
        // that relocate the surviving objects below.
        long sink = 0;
        for (int r = 0; r < rounds; r++) {
            byte[][] junk = new byte[256][];
            for (int i = 0; i < junk.length; i++) {
                junk[i] = new byte[1024];
                sink += junk[i].length;
            }
            if (sink == Long.MIN_VALUE) System.out.println("unreachable");
        }
    }

    public static void main(String[] args) {
        Object[] live = new Object[N];
        int[] before = new int[N];
        for (int i = 0; i < N; i++) {
            live[i] = new Object();
            before[i] = System.identityHashCode(live[i]);
        }

        IdentityHashMap<Object, Integer> idmap = new IdentityHashMap<>();
        Map<Object, Integer> hashmap = new HashMap<>();
        for (int i = 0; i < N; i++) {
            idmap.put(live[i], i);
            hashmap.put(live[i], i);
        }
        System.out.println("seeded " + N + " objects, idmap=" + idmap.size()
                + " hashmap=" + hashmap.size());

        for (int phase = 1; phase <= 4; phase++) {
            churn(200);
            System.gc();
            churn(50);

            int changed = 0;
            int firstChanged = -1;
            for (int i = 0; i < N; i++) {
                int now = System.identityHashCode(live[i]);
                if (now != before[i]) {
                    if (firstChanged < 0) firstChanged = i;
                    changed++;
                }
            }
            int idLost = 0, hashLost = 0;
            for (int i = 0; i < N; i++) {
                Integer a = idmap.get(live[i]);
                if (a == null || a != i) idLost++;
                Integer b = hashmap.get(live[i]);
                if (b == null || b != i) hashLost++;
            }
            System.out.println("phase " + phase
                    + " changedHashes=" + changed
                    + (firstChanged >= 0 ? " firstAt=" + firstChanged : "")
                    + " idmapLost=" + idLost
                    + " hashmapLost=" + hashLost
                    + " idmapSize=" + idmap.size());
        }

        // Zero must be impossible: HotSpot never hands out identityHashCode 0.
        int zeros = 0;
        for (int i = 0; i < N; i++) if (before[i] == 0) zeros++;
        System.out.println("zeroHashes=" + zeros);
        System.out.println("PROBE-DONE");
    }
}
