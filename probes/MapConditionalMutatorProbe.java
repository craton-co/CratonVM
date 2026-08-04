import java.util.HashMap;
import java.util.Hashtable;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * End-to-end witness for {@code Map}'s three conditional mutators —
 * {@code remove(k,v)}, {@code replace(k,v)}, {@code replace(k,old,new)} — over
 * CratonVM's native collections.
 *
 * <p>These are {@code java.util.Map} default methods that {@code HashMap} and
 * {@code Hashtable} override with bodies that walk the bucket array directly.
 * They were the only observable {@code Map} operations with no registered
 * native, so every call ran the real JDK bytecode over a table whose nodes
 * {@code native-collections} allocates. On a {@code LinkedHashMap} that path is
 * {@code HashMap.removeNode} -> {@code LinkedHashMap.afterNodeRemoval}, whose
 * first statement casts the node to {@code LinkedHashMap.Entry}, and CratonVM's
 * nodes are {@code java.util.LinkedHashMap$Node}:
 *
 * <pre>
 * ClassCastException: java.util.LinkedHashMap$Node cannot be cast to
 *                     java.util.LinkedHashMap$Entry
 * </pre>
 *
 * <p>Kafka's {@code MetadataLoader.removeAndClosePublisher} calls exactly
 * {@code publishers.remove(name, publisher)} on a {@code LinkedHashMap}, which
 * broke embedded KRaft broker shutdown in Spring Boot's
 * {@code KafkaAutoConfigurationIntegrationTests}.
 *
 * <p>Prints one line per assertion and exits non-zero on the first mismatch, so
 * the same run is meaningful on HotSpot (the control) and on CratonVM.
 *
 * <pre>
 * javac -d /tmp/probe probes/MapConditionalMutatorProbe.java
 * cratonvm --java-home &lt;jdk&gt; -cp /tmp/probe MapConditionalMutatorProbe
 * </pre>
 */
public class MapConditionalMutatorProbe {

    private static int failures = 0;

    private static void check(String what, Object expected, Object actual) {
        boolean ok = expected == null ? actual == null : expected.equals(actual);
        System.out.println((ok ? "  ok   " : "  FAIL ") + what
                + " expected=" + expected + " actual=" + actual);
        if (!ok) {
            failures++;
        }
    }

    /** Runs the whole battery against one already-empty map. */
    private static void exercise(String label, Map<String, String> m) {
        System.out.println("== " + label + " (" + m.getClass().getName() + ")");
        m.put("a", "1");
        m.put("b", "2");
        m.put("c", "3");

        // remove(k,v): value must match.
        check("remove(a,wrong)", Boolean.FALSE, m.remove("a", "999"));
        check("  a still there", "1", m.get("a"));
        check("remove(a,1)", Boolean.TRUE, m.remove("a", "1"));
        check("  a gone", null, m.get("a"));
        check("  size", Integer.valueOf(2), Integer.valueOf(m.size()));
        check("remove(absent,x)", Boolean.FALSE, m.remove("zz", "x"));
        check("  size unchanged", Integer.valueOf(2), Integer.valueOf(m.size()));

        // replace(k,v): only a present key, returns the old value.
        check("replace(b,20)", "2", m.replace("b", "20"));
        check("  b updated", "20", m.get("b"));
        check("replace(absent,x)", null, m.replace("zz", "x"));
        check("  no insert", Integer.valueOf(2), Integer.valueOf(m.size()));

        // replace(k,old,new): compare-and-set.
        check("replace(c,wrong,30)", Boolean.FALSE, m.replace("c", "999", "30"));
        check("  c unchanged", "3", m.get("c"));
        check("replace(c,3,30)", Boolean.TRUE, m.replace("c", "3", "30"));
        check("  c updated", "30", m.get("c"));
        check("  size", Integer.valueOf(2), Integer.valueOf(m.size()));

        // The insertion-order chain must have survived every one of those.
        check("  keys", "[b, c]", m.keySet().toString());
    }

    public static void main(String[] args) {
        exercise("LinkedHashMap", new LinkedHashMap<>());
        exercise("HashMap", new HashMap<>());

        // Hashtable rejects a null value in all three (Objects.requireNonNull).
        Hashtable<String, String> ht = new Hashtable<>();
        ht.put("a", "1");
        System.out.println("== Hashtable null-value contract");
        try {
            ht.remove("a", null);
            check("remove(a,null) throws NPE", Boolean.TRUE, Boolean.FALSE);
        } catch (NullPointerException expected) {
            check("remove(a,null) throws NPE", Boolean.TRUE, Boolean.TRUE);
        }
        check("  a still there", "1", ht.get("a"));
        check("remove(a,1)", Boolean.TRUE, ht.remove("a", "1"));
        check("  size", Integer.valueOf(0), Integer.valueOf(ht.size()));

        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
