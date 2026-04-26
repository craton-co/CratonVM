import java.util.*;
import java.util.stream.*;

public class TestStream {
    public static void main(String[] args) {
        System.out.println("Step 1: Arrays.asList");
        List<String> list = Arrays.asList("HELLO", "WORLD", "FOO");
        System.out.println("Step 2: size = " + list.size());

        System.out.println("Step 3: stream");
        Stream<String> s = list.stream();
        System.out.println("Step 4: count");
        long count = s.count();
        System.out.println("Step 5: count = " + count);

        System.out.println("Step 6: filter");
        List<String> filtered = list.stream()
            .filter(w -> w.length() > 3)
            .collect(Collectors.toList());
        System.out.println("Step 7: filtered = " + filtered.size());

        System.out.println("Done!");
    }
}
