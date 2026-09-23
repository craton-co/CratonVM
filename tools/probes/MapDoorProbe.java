import java.util.HashMap;

/**
 * A {@code HashMap.get}/{@code put} loop inside a single method, so the loop
 * body is compiled by the OSR door -- the door {@code H12-1} O1 converted.
 *
 * <p>Read with {@code CRATONVM_INTRINSIC_STATS=1} and look at the
 * {@code compiled collection direct-helper binds} line, specifically its
 * {@code osr_hashmap_get=} / {@code osr_hashmap_put=} fields. Those two
 * counters are {@code H12-1} N2, added 2026-09-22: before them the OSR door's
 * HashMap binds had no counter at all, which is why {@code H7-1} §6c's
 * prediction about {@code collection_direct_helper_sites()} was confirmed by
 * an instrument blind to the door doing the binding.
 *
 * <p>MEASURED 2026-09-22, this probe, one binary:
 *
 * <pre>
 *   --compatible   osr_hashmap_get=1 osr_hashmap_put=1
 *   --jdk-only     osr_hashmap_get=0 osr_hashmap_put=0
 * </pre>
 *
 * <p>The strict-mode zero is the bind being refused, not the site being
 * absent: {@code --jdk-only-report} names both triples with kind
 * {@code jit-thin-direct-helper}. Under {@code --jdk-only} the VM-side
 * {@code HashMap} overlay is not registered at all, so
 * {@code direct_helper_refusal}'s "no registration" arm fires and the real
 * JDK's own bytecode runs. Before O1 that bind was unconditional in BOTH
 * modes and only the callee-side gate stopped the helper serving.
 *
 * <p>The receiver is declared {@code HashMap}, not {@code Map}: the OSR
 * ladder's arm is {@code invokevirtual java/util/HashMap} and an interface-typed
 * variable would not reach it (H7-1).
 */
public class MapDoorProbe {
    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        HashMap<Integer, Integer> m = new HashMap<>();
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            m.put(i & 1023, i);
            Integer v = m.get(i & 1023);
            acc += v == null ? 0 : v;
        }
        System.out.println("PROBE MapDoor size=" + m.size() + " acc=" + acc);
    }
}
