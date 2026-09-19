import java.util.*;

/**
 * RC-3 repro for SC-map-multivaluemap-family: a native HashMap's equals()
 * compared against a non-native-layout Map operand (TreeMap, a custom
 * delegating Map like Spring's LinkedMultiValueMap), plus regression cases.
 */
public class MapEqRepro {

    /** Foreign Map: not natively modelled, slot 0 is a delegate field (mirrors
     *  Spring's LinkedMultiValueMap whose slot 0 is its targetMap). */
    static final class ForeignMap extends AbstractMap<String, String> {
        private final Map<String, String> delegate = new HashMap<>();
        @Override public String put(String k, String v) { return delegate.put(k, v); }
        @Override public Set<Entry<String, String>> entrySet() { return delegate.entrySet(); }
    }

    static int pass = 0, fail = 0;
    static void check(String name, boolean got, boolean want) {
        boolean ok = (got == want);
        if (ok) pass++; else fail++;
        System.out.println((ok ? "PASS " : "FAIL ") + name + " => " + got + " (want " + want + ")");
    }

    public static void main(String[] args) {
        HashMap<String, String> hm = new HashMap<>();
        hm.put("key1", "value1");

        // RC-3 core: native HashMap.equals(foreign TreeMap)
        TreeMap<String, String> tm = new TreeMap<>();
        tm.put("key1", "value1");
        check("hm.equals(treeMap)", hm.equals(tm), true);
        check("treeMap.equals(hm)", tm.equals(hm), true);

        // RC-3 core: native HashMap.equals(custom delegating Map)
        ForeignMap fm = new ForeignMap();
        fm.put("key1", "value1");
        check("hm.equals(foreignMap)", hm.equals(fm), true);

        // RC-3 core: native HashMap.equals(singletonMap)
        Map<String, String> sm = Collections.singletonMap("key1", "value1");
        check("hm.equals(singletonMap)", hm.equals(sm), true);

        // Regression: HashMap vs equal HashMap
        HashMap<String, String> hmEq = new HashMap<>();
        hmEq.put("key1", "value1");
        check("hm.equals(equalHashMap)", hm.equals(hmEq), true);

        // Regression: HashMap vs HashMap differing on value
        HashMap<String, String> hmDiff = new HashMap<>();
        hmDiff.put("key1", "DIFFERENT");
        check("hm.equals(diffHashMap)", hm.equals(hmDiff), false);

        // Regression: differing foreign value
        ForeignMap fmDiff = new ForeignMap();
        fmDiff.put("key1", "DIFFERENT");
        check("hm.equals(diffForeign)", hm.equals(fmDiff), false);

        // Regression: foreign with extra entry (size mismatch)
        ForeignMap fmBig = new ForeignMap();
        fmBig.put("key1", "value1");
        fmBig.put("key2", "value2");
        check("hm.equals(biggerForeign)", hm.equals(fmBig), false);

        // Regression: non-Map argument
        check("hm.equals(string)", hm.equals("not a map"), false);

        // Null-value contract: both map a key to null
        HashMap<String, String> n1 = new HashMap<>(); n1.put("k", null);
        HashMap<String, String> n2 = new HashMap<>(); n2.put("k", null);
        check("nullVal hm.equals(hm)", n1.equals(n2), true);

        // Null-value across foreign: TreeMap mapping k->null
        TreeMap<String, String> nt = new TreeMap<>(); nt.put("k", null);
        check("nullVal hm.equals(treeMap)", n1.equals(nt), true);

        // Null-value mismatch: this maps k->null, other maps k->"x"
        HashMap<String, String> nx = new HashMap<>(); nx.put("k", "x");
        check("nullVal vs nonNull", n1.equals(nx), false);

        // this maps k->null, other has DIFFERENT key (absent) — sizes equal(1), key absent
        HashMap<String, String> ny = new HashMap<>(); ny.put("other", null);
        check("nullVal absent-key", n1.equals(ny), false);

        // LinkedMultiValueMap-shaped: List values compared by List.equals
        HashMap<String, List<String>> lm1 = new HashMap<>();
        lm1.put("key1", Collections.singletonList("value1"));
        ForeignMapList lm2 = new ForeignMapList();
        lm2.put("key1", new ArrayList<>(Collections.singletonList("value1")));
        check("listValue hm.equals(foreign)", lm1.equals(lm2), true);

        // Empty maps
        check("empty hm.equals(empty tm)", new HashMap<>().equals(new TreeMap<>()), true);

        System.out.println("RESULT pass=" + pass + " fail=" + fail);
        if (fail != 0) System.exit(1);
    }

    static final class ForeignMapList extends AbstractMap<String, List<String>> {
        private final Map<String, List<String>> delegate = new HashMap<>();
        @Override public List<String> put(String k, List<String> v) { return delegate.put(k, v); }
        @Override public Set<Entry<String, List<String>>> entrySet() { return delegate.entrySet(); }
    }
}
