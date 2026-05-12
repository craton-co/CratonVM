import java.util.*; import java.util.stream.*;
public class SC {
    public static void main(String[] a) {
        Set<String> set = Stream.of("a","b","c").collect(Collectors.toSet());
        System.out.println("set=" + set + " size=" + set.size() + " containsA=" + set.contains("a"));
        Set<String> immut = Stream.of("a","b","c").collect(Collectors.toUnmodifiableSet());
        System.out.println("immut=" + immut + " size=" + immut.size() + " containsA=" + immut.contains("a"));
    }
}
