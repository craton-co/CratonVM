import java.util.*; import java.util.stream.*;
public class SC2 {
    public static void main(String[] a) {
        // toCollection(LinkedHashSet::new) - the pattern from ABNG line 175
        Set<String> r = Stream.of("a","b","c")
            .map(s -> s + "x")
            .collect(Collectors.toCollection(LinkedHashSet::new));
        System.out.println("r=" + r + " null?=" + (r==null) + " size=" + (r==null?-1:r.size()) + " containsAX=" + (r!=null && r.contains("ax")));
        // Also try the empty stream variant
        Set<String> empty = Stream.<String>empty().collect(Collectors.toCollection(LinkedHashSet::new));
        System.out.println("empty=" + empty + " null?=" + (empty==null));
    }
}
