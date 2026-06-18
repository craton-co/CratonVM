import java.util.List;
import java.util.stream.Stream;
public class ForEachOrderedRepro {
    public static void main(String[] a) {
        System.out.println("start");
        // 1) Stream.forEachOrdered on a sequential stream
        Stream.of("x","y","z").forEachOrdered(s -> System.out.println("ord:" + s));
        // 2) via List.stream()
        List.of(1,2,3).stream().forEachOrdered(i -> System.out.println("li:" + i));
        // 3) forEach for contrast
        Stream.of("a","b").forEach(s -> System.out.println("fe:" + s));
        System.out.println("done");
    }
}
