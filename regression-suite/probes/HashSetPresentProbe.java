import java.util.*;
import java.util.stream.*;
public class HashSetPresentProbe {
    static void chk(String tag, Set<String> s) {
        try {
            int before = s.size();
            boolean r = s.remove("b");
            System.out.println(tag + " remove(b)=" + r + " size " + before + "->" + s.size()
                               + " has_b=" + s.contains("b"));
        } catch (Throwable t) { System.out.println(tag + " threw " + t.getClass().getName()); }
    }
    public static void main(String[] a) {
        chk("ctorList ", new HashSet<>(Arrays.asList("a","b","c","d")));
        chk("ofCopy   ", new HashSet<>(Set.of("a","b","c","d")));
        Map<String,Integer> m = new HashMap<>();
        for (String k : new String[]{"a","b","c","d"}) m.put(k,1);
        chk("keySet   ", m.keySet());
        chk("collect  ", Stream.of("a","b","c","d").collect(Collectors.toSet()));
        chk("streamHS ", Stream.of("a","b","c","d").collect(Collectors.toCollection(HashSet::new)));
    }
}
