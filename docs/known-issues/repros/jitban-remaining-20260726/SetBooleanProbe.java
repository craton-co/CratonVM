import java.util.*;

// Residual sweep for the class of bug behind the @SuppressWarnings
// "duplicate element 'value'" failure: a synthetic-backed collection whose
// mutation SUCCEEDS but whose boolean return value is wrong (real HashSet
// bytecode compares the backing map's value against JDK `HashSet.PRESENT`,
// a sentinel this VM's synthetic map never stores).
//
// Every line prints a token; diff CratonVM's output against HotSpot's.
public class SetBooleanProbe {
    static StringBuilder sb = new StringBuilder();

    static void p(String k, Object v) { sb.append(k).append('=').append(v).append('\n'); }

    static Set<String> mk(String kind) {
        switch (kind) {
            case "HashSet": return new HashSet<>();
            case "LinkedHashSet": return new LinkedHashSet<>();
            case "TreeSet": return new TreeSet<>();
            default: throw new IllegalArgumentException(kind);
        }
    }

    static void exercise(String kind) {
        Set<String> s = mk(kind);
        p(kind + ".add.new", s.add("a"));
        p(kind + ".add.dup", s.add("a"));
        s.add("b"); s.add("c");
        p(kind + ".remove.present", s.remove("b"));
        p(kind + ".remove.absent", s.remove("zz"));
        p(kind + ".after.size", s.size());
        p(kind + ".after.contains.b", s.contains("b"));

        Set<String> s2 = mk(kind);
        s2.addAll(Arrays.asList("a", "b", "c", "d"));
        p(kind + ".removeAll.hit", s2.removeAll(Arrays.asList("b", "d")));
        p(kind + ".removeAll.size", s2.size());
        p(kind + ".removeAll.miss", s2.removeAll(Arrays.asList("x", "y")));

        Set<String> s3 = mk(kind);
        s3.addAll(Arrays.asList("a", "b", "c", "d"));
        p(kind + ".retainAll.hit", s3.retainAll(Arrays.asList("a", "c")));
        p(kind + ".retainAll.size", s3.size());
        p(kind + ".retainAll.noop", s3.retainAll(Arrays.asList("a", "c")));

        Set<String> s4 = mk(kind);
        s4.addAll(Arrays.asList("a", "bb", "ccc"));
        p(kind + ".removeIf.hit", s4.removeIf(x -> x.length() > 1));
        p(kind + ".removeIf.size", s4.size());
        p(kind + ".removeIf.noop", s4.removeIf(x -> x.length() > 1));

        Set<String> s5 = mk(kind);
        s5.addAll(Arrays.asList("a", "b"));
        Iterator<String> it = s5.iterator();
        it.next(); it.remove();
        p(kind + ".iter.remove.size", s5.size());

        // addAll boolean
        Set<String> s6 = mk(kind);
        p(kind + ".addAll.new", s6.addAll(Arrays.asList("a", "b")));
        p(kind + ".addAll.dup", s6.addAll(Arrays.asList("a", "b")));

        // equals/hashCode round-trip through a Set-of-Sets
        Set<String> s7 = mk(kind);
        s7.addAll(Arrays.asList("a", "b"));
        p(kind + ".equals.self", s7.equals(new HashSet<>(Arrays.asList("a", "b"))));
    }

    static void mapChecks() {
        Map<String, String> m = new HashMap<>();
        m.put("k", "v");
        p("HashMap.remove.kv.hit", m.remove("k", "v"));
        m.put("k", "v");
        p("HashMap.remove.kv.wrongval", m.remove("k", "other"));
        p("HashMap.keySet.remove", m.keySet().remove("k"));
        p("HashMap.after.size", m.size());

        Map<String, String> lm = new LinkedHashMap<>();
        lm.put("k", "v");
        p("LinkedHashMap.keySet.remove", lm.keySet().remove("k"));
        p("LinkedHashMap.after.size", lm.size());

        Properties props = new Properties();
        props.setProperty("p", "1");
        props.setProperty("q", "2");
        p("Properties.keySet.remove", props.keySet().remove("p"));
        p("Properties.after.size", props.size());
        p("Properties.after.getP", props.getProperty("p"));
    }

    public static void main(String[] args) {
        for (String kind : new String[] {"HashSet", "LinkedHashSet", "TreeSet"}) {
            exercise(kind);
        }
        mapChecks();
        System.out.print(sb);
    }
}
