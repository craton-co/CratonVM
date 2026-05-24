import java.util.Arrays;
import java.util.List;
import java.util.stream.Stream;
import java.util.stream.Collectors;

public class FlatMapProbe2 {
    public static void main(String[] args) {
        String[][] arrays = new String[][] { {"a"}, {"b", "bs"}, {"c"} };

        // 1. Direct Arrays.stream
        long n1 = Arrays.stream(arrays).count();
        System.out.println("outer-count=" + n1);

        // 2. map
        List<Integer> mapped = Arrays.stream(arrays).map(a -> a.length).collect(Collectors.toList());
        System.out.println("mapped=" + mapped);

        // 3. flatMap with lambda
        List<String> fmLambda = Arrays.stream(arrays).flatMap(a -> Arrays.stream(a)).collect(Collectors.toList());
        System.out.println("flatMap-lambda-size=" + fmLambda.size() + " list=" + fmLambda);

        // 4. flatMap with method ref
        List<String> fmRef = Arrays.stream(arrays).flatMap(Arrays::stream).collect(Collectors.toList());
        System.out.println("flatMap-ref-size=" + fmRef.size() + " list=" + fmRef);

        // 5. flatMap and forEach
        System.out.println("--forEach--");
        Arrays.stream(arrays).flatMap(a -> Arrays.stream(a)).forEach(s -> System.out.println("  el=" + s));

        // 6. Stream.of single
        List<String> ofL = Stream.of("x", "y", "z").collect(Collectors.toList());
        System.out.println("of-list=" + ofL);

        // 7. flatMap returning single-elem streams
        List<String> simpleFm = Stream.of("x", "y", "z").flatMap(s -> Stream.of(s)).collect(Collectors.toList());
        System.out.println("simpleFm=" + simpleFm);
    }
}
