import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * A dispatch witness that reports WHETHER IT CAN SEE, and runs ONE case per process.
 *
 * <p>WHY THIS EXISTS
 *
 * <p>Every witness this effort has used was a coincidence of the VM being wrong
 * in a visible way, so each one died as the VM got more correct. {@code H17}
 * measured FOUR OF SIX blind on a current binary — bucket-head class,
 * {@code modCount}, {@code hashCode()} counts and {@code equals()} counts — and
 * one of them went blind <em>because the VM was fixed</em>: {@code H16-2}
 * taught the native to mint real {@code HashMap$Node}s, so the bucket-head
 * witness now agrees in both directions. "A fix to the VM removed a witness"
 * ({@code H0-8} §7).
 *
 * <p>A witness that has gone blind does not say so. It agrees, and agreement
 * reads as "no defect". That is the failure this file is built against, so the
 * design rule here is:
 *
 * <blockquote>the instrument's job is not to be right about the VM. It is to
 * state, from the run itself, WHICH of its signals can currently tell the two
 * dispatch paths apart — and to refuse a verdict when none of them can.</blockquote>
 *
 * <p>Liveness is not decided in here. This class only EMITS signals; the
 * armed/unarmed comparison that decides which ones discriminate is
 * {@code dispatch-witness.sh}, because a single process cannot see both paths.
 *
 * <p>ONE CASE PER PROCESS, ALWAYS. {@code H0-8} retracted four "discriminators"
 * of a supposed third CHM defect that were pure case order: with anything
 * latched per process, running case 2 after case 1 applies position as a hidden
 * treatment. {@link #main} therefore takes exactly one case name and refuses to
 * run two. Do not add a loop.
 *
 * <p>THE SIGNALS, and why each is expected to outlive a correctness fix or is
 * marked as not expected to:
 *
 * <ul>
 *   <li>{@code frames} — the caller frames captured INSIDE a key's
 *       {@code hashCode()}. If real bytecode ran, the callback happens from
 *       inside {@code java.util.HashMap} frames; a native has no reason to
 *       reproduce the JDK's internal frame names, and reproducing them is not
 *       what "more correct" means. DISPATCH-SIDE: it asks which code ran, not
 *       what it computed.
 *   <li>{@code hccount}, {@code eqcount} — how many times the key's callbacks
 *       ran. VALUE-SIDE and already measured blind by {@code H17}; kept so the
 *       harness can SHOW them blind rather than have someone re-derive it.
 *   <li>{@code table}, {@code head}, {@code modcount} — reflective reads of
 *       {@code HashMap}'s internals. VALUE-SIDE, and {@code head} is the one
 *       {@code H16-2} killed. Kept for the same reason.
 *   <li>{@code iter} — the entry-set iterator's class name.
 *   <li>{@code consistent} — {@code size()} against {@code keySet().size()} and
 *       {@code entrySet().size()}. This is {@code H13-1}'s primary symptom and
 *       the one thing in this list that is a DEFECT rather than a signal.
 * </ul>
 *
 * <p>Every line is {@code W <signal> <value>} so the driver can diff two runs
 * without parsing prose, and an unavailable signal prints {@code W <name> n/a:<why>}
 * rather than being omitted — a MISSING line and an EQUAL line would otherwise
 * both read as "did not discriminate".
 *
 * <p>Usage: {@code java DispatchWitness <case>} where case is one of
 * {@code put}, {@code get}, {@code iterate}. Run it under
 * {@code CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap} and again without.
 */
public class DispatchWitness {

    /** Recorded by the key below; read after the operation under test. */
    static final List<String> HASH_FRAMES = new ArrayList<>();
    static int hashCalls = 0;
    static int equalsCalls = 0;

    /**
     * A key that reports where it was called FROM, not just how often.
     *
     * <p>{@code hashCode()} is the one JDK-internal call site an application can
     * legally stand inside, which is what makes the frame list reachable at all.
     */
    static final class TellTaleKey {
        final int id;

        TellTaleKey(int id) {
            this.id = id;
        }

        @Override
        public int hashCode() {
            hashCalls++;
            if (HASH_FRAMES.isEmpty()) {
                for (StackTraceElement e : Thread.currentThread().getStackTrace()) {
                    HASH_FRAMES.add(e.getClassName() + "." + e.getMethodName());
                }
            }
            return id;
        }

        @Override
        public boolean equals(Object o) {
            equalsCalls++;
            return o instanceof TellTaleKey && ((TellTaleKey) o).id == id;
        }

        @Override
        public String toString() {
            return "K" + id;
        }
    }

    static void w(String name, String value) {
        System.out.println("W " + name + " " + value);
    }

    /** A reflective read that reports WHY it failed instead of vanishing. */
    static Object peek(Map<?, ?> m, String field) throws Exception {
        Field f = HashMap.class.getDeclaredField(field);
        f.setAccessible(true);
        return f.get(m);
    }

    static void reflectiveSignals(HashMap<?, ?> m) {
        try {
            Object table = peek(m, "table");
            w("table", table == null ? "null" : table.getClass().getName());
            if (table instanceof Object[]) {
                Object[] t = (Object[]) table;
                String head = "none";
                for (Object o : t) {
                    if (o != null) {
                        head = o.getClass().getName();
                        break;
                    }
                }
                w("head", head);
                w("tablen", String.valueOf(t.length));
            } else {
                w("head", "n/a:table-is-not-an-Object-array");
                w("tablen", "n/a:table-is-not-an-Object-array");
            }
        } catch (Throwable t) {
            // One reason, three signals: say so on each, so a diff of the two
            // runs never shows a signal simply absent on one side.
            String why = "n/a:" + t.getClass().getSimpleName();
            w("table", why);
            w("head", why);
            w("tablen", why);
        }
        try {
            w("modcount", String.valueOf(peek(m, "modCount")));
        } catch (Throwable t) {
            w("modcount", "n/a:" + t.getClass().getSimpleName());
        }
    }

    static void frameSignals() {
        StringBuilder jdk = new StringBuilder();
        int n = 0;
        for (String f : HASH_FRAMES) {
            // Only the JDK-internal frames matter: the probe's own frames are
            // identical on both paths by construction and would swamp the diff.
            if (f.startsWith("java.util.") || f.startsWith("jdk.internal.")) {
                if (n++ > 0) {
                    jdk.append(',');
                }
                jdk.append(f);
            }
        }
        w("frames", n == 0 ? "none" : jdk.toString());
        w("framedepth", String.valueOf(HASH_FRAMES.size()));
    }

    /**
     * The size/view agreement, and the iterator's class.
     *
     * <p>Every signal is individually guarded. MEASURED 2026-08-21: under
     * {@code CRATONVM_LOADER=enforce-native-shadow} scoped to
     * {@code java/util/HashMap}, {@code entrySet().iterator()} throws an NPE
     * from {@code HashMap$EntryIterator.<init>} — so an unguarded call here
     * killed the process AFTER the frame signals printed and BEFORE
     * {@code iter}, which the driver would then have read as "the signal was
     * absent on one side". A witness must survive the VM being wrong, not only
     * the VM being right.
     */
    static void consistency(Map<?, ?> m) {
        int size = -1;
        String ks = "n/a";
        String es = "n/a";
        try {
            size = m.size();
        } catch (Throwable t) {
            w("throws.size", t.getClass().getName());
        }
        try {
            ks = String.valueOf(m.keySet().size());
        } catch (Throwable t) {
            w("throws.keySet", t.getClass().getName());
        }
        try {
            es = String.valueOf(m.entrySet().size());
        } catch (Throwable t) {
            w("throws.entrySet", t.getClass().getName());
        }
        String s = String.valueOf(size);
        w("consistent", (s.equals(ks) && s.equals(es)) ? "yes" : "NO");
        w("sizes", s + "/" + ks + "/" + es);
        try {
            w("iter", m.entrySet().iterator().getClass().getName());
        } catch (Throwable t) {
            // NOT an absence. `iter` is emitted either way so a two-run diff
            // compares two values instead of a value against nothing.
            w("iter", "throws:" + t.getClass().getName());
        }
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            // H0-8: a probe that runs several cases in one process is a
            // repeated-measures design, and anything latched per process is
            // confounded with case order. Refusing is cheaper than a retraction.
            System.out.println("W error one-case-per-process:"
                    + " usage DispatchWitness <put|get|iterate>");
            System.exit(2);
        }
        String c = args[0];
        HashMap<TellTaleKey, String> m = new HashMap<>();
        // The operation under test is guarded too: a throw here must still leave
        // the signals it did reach on stdout, and must publish ITSELF as a
        // signal. `W op` is the strongest witness of all when it differs.
        String op = "ok";
        try {
            switch (c) {
                case "put":
                    for (int i = 0; i < 8; i++) {
                        m.put(new TellTaleKey(i), "v" + i);
                    }
                    break;
                case "get":
                    for (int i = 0; i < 8; i++) {
                        m.put(new TellTaleKey(i), "v" + i);
                    }
                    HASH_FRAMES.clear();
                    hashCalls = 0;
                    equalsCalls = 0;
                    for (int i = 0; i < 8; i++) {
                        m.get(new TellTaleKey(i));
                    }
                    break;
                case "iterate":
                    for (int i = 0; i < 8; i++) {
                        m.put(new TellTaleKey(i), "v" + i);
                    }
                    HASH_FRAMES.clear();
                    hashCalls = 0;
                    equalsCalls = 0;
                    int seen = 0;
                    for (Map.Entry<TellTaleKey, String> e : m.entrySet()) {
                        seen += e.getKey().id >= 0 ? 1 : 0;
                    }
                    w("iterated", String.valueOf(seen));
                    break;
                default:
                    System.out.println("W error unknown-case:" + c);
                    System.exit(2);
            }
        } catch (Throwable t) {
            op = "throws:" + t.getClass().getName();
        }
        w("case", c);
        w("op", op);
        w("hccount", String.valueOf(hashCalls));
        w("eqcount", String.valueOf(equalsCalls));
        frameSignals();
        reflectiveSignals(m);
        consistency(m);
        // The banner is the driver's proof that the process reached the end and
        // that a missing signal above is a real `n/a`, not a truncated stdout.
        System.out.println("PASS DispatchWitness (case=" + c + ", 13 signals)");
    }
}
