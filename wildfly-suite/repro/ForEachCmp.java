import java.util.stream.Stream;
public class ForEachCmp {
    public static void main(String[] a) {
        System.out.println("A forEach:");
        Stream.of("x","y").forEach(s -> System.out.println("  fe:" + s));
        System.out.println("B map+forEach:");
        Stream.of(1,2).map(i -> i*10).forEach(s -> System.out.println("  m:" + s));
        System.out.println("C forEachOrdered:");
        Stream.of("p","q").forEachOrdered(s -> System.out.println("  ord:" + s));
        System.out.println("done");
    }
}
