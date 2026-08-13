import java.util.*;
import java.util.concurrent.ConcurrentHashMap;

/**
 * getClass() of every collection VIEW, against a HotSpot oracle.
 *
 * One line per expression: `NAME=<binary class name>`. Diff CratonVM's output
 * against HotSpot's; every differing line is a carrier-class divergence.
 */
public final class ViewClassProbe {
    static String cn(Object o) { return o == null ? "null" : o.getClass().getName(); }

    public static void main(String[] a) {
        LinkedHashMap<String, String> lhm = new LinkedHashMap<>();
        lhm.put("a", "1"); lhm.put("b", "2");
        HashMap<String, String> hm = new HashMap<>(lhm);
        TreeMap<String, String> tm = new TreeMap<>(lhm);
        Hashtable<String, String> ht = new Hashtable<>(lhm);
        ConcurrentHashMap<String, String> chm = new ConcurrentHashMap<>(lhm);
        ArrayList<String> al = new ArrayList<>(List.of("x", "y", "z"));
        LinkedHashSet<String> lhs = new LinkedHashSet<>(List.of("p", "q"));
        TreeSet<String> ts = new TreeSet<>(List.of("p", "q"));

        System.out.println("hm.values=" + cn(hm.values()));
        System.out.println("hm.keySet=" + cn(hm.keySet()));
        System.out.println("hm.entrySet=" + cn(hm.entrySet()));
        System.out.println("lhm.values=" + cn(lhm.values()));
        System.out.println("lhm.keySet=" + cn(lhm.keySet()));
        System.out.println("lhm.entrySet=" + cn(lhm.entrySet()));
        System.out.println("tm.values=" + cn(tm.values()));
        System.out.println("tm.keySet=" + cn(tm.keySet()));
        System.out.println("tm.entrySet=" + cn(tm.entrySet()));
        System.out.println("ht.values=" + cn(ht.values()));
        System.out.println("ht.keySet=" + cn(ht.keySet()));
        System.out.println("chm.values=" + cn(chm.values()));
        System.out.println("chm.keySet=" + cn(chm.keySet()));
        System.out.println("al.subList=" + cn(al.subList(0, 2)));
        System.out.println("arrays.asList=" + cn(Arrays.asList("a")));
        System.out.println("unmodList=" + cn(Collections.unmodifiableList(al)));
        System.out.println("lhs.iterator=" + cn(lhs.iterator()));
        System.out.println("ts.descendingSet=" + cn(ts.descendingSet()));

        // Behavioural consequences of the carrier class, independent of names.
        System.out.println("hm.values instanceof List=" + (hm.values() instanceof List));
        System.out.println("lhm.values instanceof List=" + (lhm.values() instanceof List));
        System.out.println("hm.values equals list=" + hm.values().equals(new ArrayList<>(hm.values())));

        // The view must stay LIVE and correct after the carrier change.
        Collection<String> v = lhm.values();
        System.out.println("v.size=" + v.size() + " contains2=" + v.contains("2"));
        lhm.put("c", "3");
        System.out.println("v.afterPut.size=" + v.size() + " contains3=" + v.contains("3"));
        v.remove("1");
        System.out.println("lhm.afterViewRemove=" + lhm);
        Set<String> ks = lhm.keySet();
        ks.remove("b");
        System.out.println("lhm.afterKeySetRemove=" + lhm);
        Iterator<String> it = lhm.values().iterator();
        it.next(); it.remove();
        System.out.println("lhm.afterIterRemove=" + lhm);
        System.out.println("values.toString=" + new LinkedHashMap<>(Map.of("k", "v")).values());
        Object[] arr = hm.values().toArray();
        System.out.println("values.toArray.len=" + arr.length + " cls=" + cn(arr));
        System.out.println("values.stream.count=" + hm.values().stream().count());
        List<String> copy = new ArrayList<>(hm.values());
        System.out.println("copyOfValues.size=" + copy.size());
    }
}
