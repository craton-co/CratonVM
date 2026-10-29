import java.util.Map;
import java.util.TreeSet;
import java.util.concurrent.ConcurrentHashMap;

/**
 * ONE CASE PER PROCESS. Do not add a loop over the cases.
 *
 * <p>RETRACTION, and why this file has an argument now.
 *
 * <p>The first version of this probe ran all six cases back to back in one
 * process and published four "discriminators" of a supposed third
 * {@code ConcurrentHashMap} defect ({@code H13-1}):
 *
 * <ol>
 *   <li>four back-to-back puts give {@code size()==1} with an empty keySet;
 *   <li>interposing {@code size()} between the puts fixes it;
 *   <li>interposing {@code Math.abs(1)} — which does nothing to the map — also
 *       fixes it;
 *   <li>{@code Integer} keys are correct where {@code String} keys are not;
 *   <li>the same map answers differently through a {@code ConcurrentHashMap}
 *       local than through a {@code Map} local.
 * </ol>
 *
 * <p><b>{@code H0-8} measured all four of 2–5 and every one was an artifact of
 * the order the cases ran in.</b> Swap two cases and the failure follows the
 * POSITION, not the treatment: with {@code Math.abs} first, {@code Math.abs}
 * fails and back-to-back passes; with {@code Integer} first, {@code Integer}
 * keys break and {@code String} keys are fine; whichever read runs first is the
 * one that is right. The sibling controls
 * {@code ChmOrderConfound{OrderTest,KeyOrder,DoorOrder}} are the swapped arms
 * that demonstrate it, and they take the same kind of argument this class now
 * does.
 *
 * <p>The rule, which is the reusable part:
 *
 * <blockquote>a probe whose cases run in sequence inside one process is a
 * repeated-measures design, and anything latched per process is confounded with
 * case order. Run each case in its own process, or randomise and repeat.</blockquote>
 *
 * <p>{@code H17-2} then named the latch: {@code jdk_only_enforce_shadow_for}
 * had exactly ONE live call site, inside {@code resolve_step1_native}, so
 * arming a class armed only its COLD, step-1 dispatches. The first use of a
 * call site was cold and was dialled; later uses hit the warm cache and were
 * not. Case order was therefore a hidden treatment on every case after the
 * first, which is exactly what the retracted discriminators were measuring.
 *
 * <p><b>That latch was FIXED on 2026-08-21</b> — all fourteen dispatch doors
 * consult the dial now, and three source-witness tests in
 * {@code native_override.rs} keep them consulting it. So on a current binary
 * this particular confound is gone, and the retraction above should be read as
 * a record of what the old measurements were, not as a live hazard.
 *
 * <p><b>The one-case-per-process rule stands anyway, and that is the point.</b>
 * The dial was never the only thing latched per process — JIT tier-up, class
 * initialisation and the invoke cache's own population all are — so a
 * repeated-measures probe is confounded whether or not any particular latch has
 * been fixed. A design that is only correct because one named bug is currently
 * absent is not a correct design.
 *
 * <p><b>What survives the retraction:</b> the primary symptom is real.
 * {@code size()==1} with an empty {@code keySet()} on an armed CHM is an
 * observed divergence from HotSpot, and its traced consequence stands —
 * {@code CryptoPermissions.isEmpty()} true for a map whose {@code size()} is 1,
 * so {@code JceSecurity.<clinit>} throws and the JCE is dead for the process.
 * The unarmed control was correct in every one of those runs.
 *
 * <p>Run it with {@code chm-consistency.sh}, which drives every case in its own
 * process and repeats the set in a rotated order.
 *
 * <p>Usage: {@code java ChmConsistencyProbe <case>}, case one of
 * {@code plain}, {@code withsize}, {@code withabs}, {@code intkeys},
 * {@code viachm}, {@code viamap}.
 */
public class ChmConsistencyProbe {

    static final String CASES = "plain withsize withabs intkeys viachm viamap";

    static void report(String tag, ConcurrentHashMap<?, ?> m) {
        System.out.println("CK ChmConsistencyProbe " + tag
                + " size=" + m.size() + " keys=" + new TreeSet<>(m.keySet()));
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            // Refusing is cheaper than a retraction. See the class comment.
            System.out.println("CK ChmConsistencyProbe error=one-case-per-process"
                    + " usage=ChmConsistencyProbe<" + CASES.replace(' ', '|') + ">");
            System.exit(2);
        }
        String c = args[0];
        switch (c) {
            case "plain": {
                ConcurrentHashMap<String, String> m = new ConcurrentHashMap<>();
                for (int i = 0; i < 4; i++) {
                    m.put("k" + i, "v" + i);
                }
                report("back-to-back", m);
                break;
            }
            case "withsize": {
                ConcurrentHashMap<String, String> m = new ConcurrentHashMap<>();
                for (int i = 0; i < 4; i++) {
                    m.put("k" + i, "v" + i);
                    m.size();
                }
                report("puts+size", m);
                break;
            }
            case "withabs": {
                ConcurrentHashMap<String, String> m = new ConcurrentHashMap<>();
                for (int i = 0; i < 4; i++) {
                    m.put("k" + i, "v" + i);
                    Math.abs(1);
                }
                report("puts+abs", m);
                break;
            }
            case "intkeys": {
                ConcurrentHashMap<Integer, String> m = new ConcurrentHashMap<>();
                for (int i = 0; i < 4; i++) {
                    m.put(i, "v" + i);
                }
                report("int-keys", m);
                break;
            }
            case "viachm": {
                ConcurrentHashMap<String, String> m = new ConcurrentHashMap<>();
                m.putIfAbsent("only", "x");
                System.out.println("CK ChmConsistencyProbe via-chm containsKey="
                        + m.containsKey("only") + " keySet=" + m.keySet()
                        + " es=" + m.entrySet().size());
                break;
            }
            case "viamap": {
                ConcurrentHashMap<String, String> chm = new ConcurrentHashMap<>();
                chm.putIfAbsent("only", "x");
                // The ONLY read in this process, and it is through the Map-typed
                // local. In the retracted version this ran second and its
                // "disagreement" with the CHM-typed read above was position.
                Map<String, String> m = chm;
                System.out.println("CK ChmConsistencyProbe via-map containsKey="
                        + m.containsKey("only") + " keySet=" + m.keySet()
                        + " es=" + m.entrySet().size());
                break;
            }
            default:
                System.out.println("CK ChmConsistencyProbe error=unknown-case:" + c);
                System.exit(2);
        }
        System.out.println("PASS ChmConsistencyProbe (case=" + c + ", 1 check)");
    }
}
