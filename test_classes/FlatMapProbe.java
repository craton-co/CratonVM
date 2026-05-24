import java.util.Arrays;
import java.util.List;
import java.util.stream.Collectors;

public class FlatMapProbe {
    public static void main(String[] args) {
        // Simulate Flink TimeUtils$TimeUnit ctor: Arrays.stream(varargs).flatMap(Arrays::stream).collect(toList)
        String[][] arrays = new String[][] { {"a"}, {"b", "bs"}, {"c"} };
        List<String> labels = Arrays.stream(arrays)
                .flatMap(Arrays::stream)
                .collect(Collectors.toList());
        System.out.println("flatMap-size=" + labels.size());
        System.out.println("flatMap-list=" + labels);

        // Same idiom that getAllUnits uses: map + Collectors.joining(",")
        String joined = labels.stream()
                .map(s -> "[" + s + "]")
                .collect(Collectors.joining(","));
        System.out.println("joined=" + joined);
    }
}
