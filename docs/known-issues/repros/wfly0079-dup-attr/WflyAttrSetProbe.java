import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Reproduces the exact set manipulation performed by WildFly 32's
 * org.jboss.as.txn.subsystem.TransactionSubsystemRootResourceDefinition
 * .registerAttributes():
 *
 *   Set<AttributeDefinition> attributes = new HashSet<>(Arrays.asList(add_attributes));
 *   attributes.remove(USE_HORNETQ_STORE_PARAM);
 *   ... 11 more removes ...
 *   for (AttributeDefinition a : attributes) mrr.registerReadWriteAttribute(a, ...);
 *   ... then the 12 removed ones are registered explicitly ...
 *
 * If any of the 12 removes fails to delete its element, that element is
 * registered twice and WildFly throws
 *   WFLYCTL0043: An attribute named 'X' is already registered at location
 *   '/subsystem=transactions'
 * wrapped in WFLYCTL0079. AttributeDefinition.hashCode() is name.hashCode()
 * and equals() is name.equals(), so this probe models it exactly.
 */
public class WflyAttrSetProbe {

    static final class Attr {
        final String name;

        Attr(String name) {
            this.name = name;
        }

        @Override
        public int hashCode() {
            return name.hashCode();
        }

        @Override
        public boolean equals(Object o) {
            if (!(o instanceof Attr)) {
                return false;
            }
            return name.equals(((Attr) o).name);
        }

        @Override
        public String toString() {
            return name;
        }
    }

    /** add_attributes, in declaration order (28 entries). */
    static final String[] ADD_NAMES = {
        "socket-binding",
        "status-socket-binding",
        "recovery-listener",
        "node-identifier",
        "process-id-uuid",
        "process-id-socket-binding",
        "process-id-socket-max-ports",
        "statistics-enabled",
        "enable-tsm-status",
        "default-timeout",
        "maximum-timeout",
        "object-store-relative-to",
        "object-store-path",
        "jts",
        "use-hornetq-store",
        "use-journal-store",
        "use-jdbc-store",
        "jdbc-store-datasource",
        "jdbc-action-store-drop-table",
        "jdbc-action-store-table-prefix",
        "jdbc-communication-store-drop-table",
        "jdbc-communication-store-table-prefix",
        "jdbc-state-store-drop-table",
        "jdbc-state-store-table-prefix",
        "journal-store-enable-async-io",
        "enable-statistics",
        "hornetq-store-enable-async-io",
        "stale-transaction-time",
    };

    /** The 12 attributes registerAttributes() removes, in bytecode order. */
    static final int[] REMOVE_IDX = {14, 15, 16, 7, 9, 10, 17, 4, 5, 6, 25, 26};

    /**
     * The same 12 names in the order registerAttributes() re-registers them
     * explicitly after the set loop. 15/14 stand in for USE_JOURNAL_STORE /
     * USE_HORNETQ_STORE, which carry the same names as the *_PARAM entries.
     * 'hornetq-store-enable-async-io' is registered LAST — which is why it is
     * the name that shows up in WFLYCTL0043.
     */
    static final int[] EXPLICIT_ORDER = {15, 16, 9, 10, 17, 4, 5, 6, 7, 25, 14, 26};

    /** Static, shared, warm instances — matches the WildFly static finals. */
    static final Attr[] STATIC_ADD = new Attr[ADD_NAMES.length];
    static {
        for (int i = 0; i < ADD_NAMES.length; i++) {
            STATIC_ADD[i] = new Attr(ADD_NAMES[i]);
        }
    }

    static final boolean SELFTEST = System.getProperty("cvm.probe.selftest") != null;

    static final AtomicLong iterations = new AtomicLong();
    static final AtomicLong failures = new AtomicLong();
    static volatile Object sink;

    static Attr[] freshAttrs() {
        Attr[] a = new Attr[ADD_NAMES.length];
        for (int i = 0; i < ADD_NAMES.length; i++) {
            // new String(...) so the hash cache starts cold every iteration,
            // exactly like a fresh <clinit> of the WildFly class.
            a[i] = new Attr(new String(ADD_NAMES[i].toCharArray()));
        }
        return a;
    }

    /** name.hashCode() for each entry, captured once on a cold interpreter. */
    static final int[] EXPECTED_HASH = new int[ADD_NAMES.length];
    static {
        for (int i = 0; i < ADD_NAMES.length; i++) {
            EXPECTED_HASH[i] = ADD_NAMES[i].hashCode();
        }
    }

    /** Returns null on success, or a description of the duplicate on failure. */
    static String oneRound(Attr[] attrs) {
        // Hash stability: AttributeDefinition.hashCode() is name.hashCode().
        // If String.hashCode() ever disagrees with itself (interpreter vs JIT
        // intrinsic, or a corrupted hash cache) the set lookup goes to the
        // wrong bucket and remove() silently misses.
        for (int i = 0; i < attrs.length; i++) {
            int nh = attrs[i].name.hashCode();
            int ah = attrs[i].hashCode();
            if (nh != EXPECTED_HASH[i] || ah != EXPECTED_HASH[i]) {
                return "HASH-DRIFT idx=" + i + " name=" + attrs[i].name
                        + " nameHash=" + nh + " attrHash=" + ah
                        + " expected=" + EXPECTED_HASH[i];
            }
        }
        Set<Attr> attributes = new HashSet<>(Arrays.asList(attrs));
        for (int idx : REMOVE_IDX) {
            // NEGATIVE CONTROL: with -Dcvm.probe.selftest=1 the
            // hornetq-store-enable-async-io remove is skipped, which is exactly
            // the state a real failure would leave behind. The probe MUST then
            // report it — otherwise a clean run proves nothing.
            if (SELFTEST && idx == 26) {
                continue;
            }
            attributes.remove(attrs[idx]);
        }

        // Model ConcreteResourceRegistration.storeAttribute: a
        // Map<String, AttributeAccess> guarded by containsKey -> throw. Iterate
        // the set (as WildFly's loop does), then add the 12 explicitly
        // registered ones in WildFly's order.
        Map<String, Object> registered = new HashMap<>();
        List<String> dupes = null;
        for (Attr a : attributes) {
            if (registered.containsKey(a.name)) {
                if (dupes == null) {
                    dupes = new ArrayList<>();
                }
                dupes.add("loop:" + a.name);
            }
            registered.put(a.name, a);
        }
        for (int idx : EXPLICIT_ORDER) {
            String n = attrs[idx].name;
            if (registered.containsKey(n)) {
                if (dupes == null) {
                    dupes = new ArrayList<>();
                }
                dupes.add("explicit:" + n);
            }
            registered.put(n, attrs[idx]);
        }
        if (dupes == null && attributes.size() == ADD_NAMES.length - REMOVE_IDX.length) {
            return null;
        }
        StringBuilder sb = new StringBuilder();
        sb.append("size=").append(attributes.size())
          .append(" expected=").append(ADD_NAMES.length - REMOVE_IDX.length)
          .append(" dupes=").append(dupes);
        sb.append(" survivors=[");
        for (int idx : REMOVE_IDX) {
            for (Attr a : attributes) {
                if (a.name.equals(attrs[idx].name)) {
                    sb.append(attrs[idx].name)
                      .append("(h=").append(attrs[idx].name.hashCode())
                      .append(",ah=").append(attrs[idx].hashCode())
                      .append(",same=").append(a == attrs[idx])
                      .append(",eq=").append(a.equals(attrs[idx]))
                      .append(") ");
                    break;
                }
            }
        }
        sb.append("]");
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        long rounds = args.length > 1 ? Long.parseLong(args[1]) : 200_000L;
        // 0 = static warm attrs, 1 = fresh cold attrs, 2 = alternate
        int mode = args.length > 2 ? Integer.parseInt(args[2]) : 2;
        int garbageKb = args.length > 3 ? Integer.parseInt(args[3]) : 8;

        System.out.println("WflyAttrSetProbe threads=" + threads + " rounds=" + rounds
                + " mode=" + mode + " garbageKb=" + garbageKb);

        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int tid = t;
            ts[t] = new Thread(() -> {
                for (long i = 0; i < rounds; i++) {
                    boolean fresh = (mode == 1) || (mode == 2 && ((i + tid) & 1) == 0);
                    Attr[] attrs = fresh ? freshAttrs() : STATIC_ADD;
                    String bad = oneRound(attrs);
                    if (bad != null) {
                        failures.incrementAndGet();
                        System.out.println("FAIL t=" + tid + " i=" + i
                                + " fresh=" + fresh + " " + bad);
                        System.out.flush();
                    }
                    iterations.incrementAndGet();
                    if (garbageKb > 0) {
                        // allocation pressure so a young GC can land inside the
                        // hashCode()/equals() dispatch of the set operations
                        sink = new byte[garbageKb * 1024];
                    }
                }
            }, "probe-" + t);
            ts[t].setDaemon(true);
            ts[t].start();
        }

        Thread reporter = new Thread(() -> {
            try {
                while (true) {
                    Thread.sleep(10_000);
                    System.out.println("... iterations=" + iterations.get()
                            + " failures=" + failures.get());
                    System.out.flush();
                }
            } catch (InterruptedException ignored) {
            }
        }, "reporter");
        reporter.setDaemon(true);
        reporter.start();

        for (Thread t : ts) {
            t.join();
        }
        System.out.println("DONE iterations=" + iterations.get()
                + " failures=" + failures.get());
        System.out.println(failures.get() == 0 ? "PASS" : "PROBE-FAILED");
    }
}
