import java.util.*;
import java.util.concurrent.*;
import java.util.function.Predicate;

public class ViewRemoveIfProbe {
    static int fails = 0;

    static <K,V> Map<K,V> fill(Map<K,V> m, K[] ks, V[] vs) {
        for (int i = 0; i < ks.length; i++) m.put(ks[i], vs[i]);
        return m;
    }

    static Map<String,String> mk(String kind) {
        Map<String,String> m;
        switch (kind) {
            case "HashMap": m = new HashMap<>(); break;
            case "LinkedHashMap": m = new LinkedHashMap<>(); break;
            case "TreeMap": m = new TreeMap<>(); break;
            case "Hashtable": m = new Hashtable<>(); break;
            case "ConcurrentHashMap": m = new ConcurrentHashMap<>(); break;
            case "ConcurrentSkipListMap": m = new ConcurrentSkipListMap<>(); break;
            default: throw new IllegalArgumentException(kind);
        }
        m.put("a","1"); m.put("b","2"); m.put("c","3");
        return m;
    }

    static void check(String name, String got, String want) {
        boolean ok = got.equals(want);
        if (!ok) fails++;
        System.out.println((ok ? "OK   " : "FAIL ") + name + " => " + got + (ok ? "" : "  (want " + want + ")"));
    }

    static String snap(Map<String,String> m) {
        TreeMap<String,String> t = new TreeMap<>(m);
        return t.toString();
    }

    static void run(String kind) {
        // entrySet().removeIf
        try {
            Map<String,String> m = mk(kind);
            boolean r = m.entrySet().removeIf(e -> e.getKey().equals("b"));
            check(kind + ".entrySet().removeIf", r + " " + snap(m), "true {a=1, c=3}");
        } catch (Throwable t) { fails++; System.out.println("FAIL " + kind + ".entrySet().removeIf threw " + t); }
        // entrySet().removeIf that matches nothing
        try {
            Map<String,String> m = mk(kind);
            boolean r = m.entrySet().removeIf(e -> e.getValue().equals("zz"));
            check(kind + ".entrySet().removeIf(none)", r + " " + snap(m), "false {a=1, b=2, c=3}");
        } catch (Throwable t) { fails++; System.out.println("FAIL " + kind + ".entrySet().removeIf(none) threw " + t); }
        // keySet().removeIf
        try {
            Map<String,String> m = mk(kind);
            boolean r = m.keySet().removeIf(k -> k.equals("b"));
            check(kind + ".keySet().removeIf", r + " " + snap(m), "true {a=1, c=3}");
        } catch (Throwable t) { fails++; System.out.println("FAIL " + kind + ".keySet().removeIf threw " + t); }
        // values().removeIf
        try {
            Map<String,String> m = mk(kind);
            boolean r = m.values().removeIf(v -> v.equals("2"));
            check(kind + ".values().removeIf", r + " " + snap(m), "true {a=1, c=3}");
        } catch (Throwable t) { fails++; System.out.println("FAIL " + kind + ".values().removeIf threw " + t); }
        // entrySet().removeAll / retainAll via a set of entries
        try {
            Map<String,String> m = mk(kind);
            Set<Map.Entry<String,String>> es = m.entrySet();
            Iterator<Map.Entry<String,String>> it = es.iterator();
            int n = 0; while (it.hasNext()) { Map.Entry<String,String> e = it.next(); if (e.getKey().equals("b")) { it.remove(); n++; } }
            check(kind + ".entrySet().iterator().remove", n + " " + snap(m), "1 {a=1, c=3}");
        } catch (Throwable t) { fails++; System.out.println("FAIL " + kind + ".entrySet().iterator().remove threw " + t); }
        // removeIf on a Set typed statically as Set (interface dispatch), removing all
        try {
            Map<String,String> m = mk(kind);
            boolean r = m.entrySet().removeIf(e -> true);
            check(kind + ".entrySet().removeIf(all)", r + " " + snap(m) + " size=" + m.size(), "true {} size=0");
        } catch (Throwable t) { fails++; System.out.println("FAIL " + kind + ".entrySet().removeIf(all) threw " + t); }
    }

    public static void main(String[] args) {
        for (String k : new String[]{"HashMap","LinkedHashMap","TreeMap","Hashtable","ConcurrentHashMap","ConcurrentSkipListMap"}) {
            System.out.println("== " + k);
            run(k);
        }
        // plain sets
        System.out.println("== plain collections");
        try {
            Set<String> s = new HashSet<>(Arrays.asList("a","b","c"));
            boolean r = s.removeIf(x -> x.equals("b"));
            check("HashSet.removeIf", r + " " + new TreeSet<>(s), "true [a, c]");
        } catch (Throwable t) { fails++; System.out.println("FAIL HashSet.removeIf threw " + t); }
        try {
            Set<String> s = ConcurrentHashMap.newKeySet();
            s.addAll(Arrays.asList("a","b","c"));
            boolean r = s.removeIf(x -> x.equals("b"));
            check("CHM.newKeySet.removeIf", r + " " + new TreeSet<>(s), "true [a, c]");
        } catch (Throwable t) { fails++; System.out.println("FAIL CHM.newKeySet.removeIf threw " + t); }
        try {
            Set<String> s = new LinkedHashSet<>(Arrays.asList("a","b","c"));
            boolean r = s.removeIf(x -> x.equals("b"));
            check("LinkedHashSet.removeIf", r + " " + new TreeSet<>(s), "true [a, c]");
        } catch (Throwable t) { fails++; System.out.println("FAIL LinkedHashSet.removeIf threw " + t); }
        try {
            Set<String> s = new TreeSet<>(Arrays.asList("a","b","c"));
            boolean r = s.removeIf(x -> x.equals("b"));
            check("TreeSet.removeIf", r + " " + new TreeSet<>(s), "true [a, c]");
        } catch (Throwable t) { fails++; System.out.println("FAIL TreeSet.removeIf threw " + t); }
        System.out.println("TOTALFAILS=" + fails);
    }
}
