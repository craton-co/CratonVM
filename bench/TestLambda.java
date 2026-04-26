import java.util.*;
import java.util.stream.*;
import java.util.function.*;

public class TestLambda {
    public static void main(String[] args) {
        int pass = 0;

        // 1. Lambda with Comparator
        List<String> names = new ArrayList<>();
        names.add("Charlie"); names.add("Alice"); names.add("Bob");
        names.sort((a, b) -> a.compareTo(b));
        if ("Alice".equals(names.get(0))) { System.out.println("PASS: sort lambda"); pass++; }
        else System.out.println("FAIL: sort=" + names.get(0));

        // 2. Stream map + collect
        List<String> upper = names.stream()
            .map(s -> s.toUpperCase())
            .collect(Collectors.toList());
        if ("ALICE".equals(upper.get(0))) { System.out.println("PASS: stream map"); pass++; }
        else System.out.println("FAIL: map=" + upper.get(0));

        // 3. Stream filter + count
        long count = names.stream().filter(s -> s.length() > 3).count();
        if (count == 2) { System.out.println("PASS: filter count"); pass++; }
        else System.out.println("FAIL: count=" + count);

        // 4. Predicate
        Predicate<Integer> isEven = n -> n % 2 == 0;
        if (isEven.test(4) && !isEven.test(3)) { System.out.println("PASS: predicate"); pass++; }
        else System.out.println("FAIL: predicate");

        // 5. Function composition
        Function<Integer, Integer> doubleIt = n -> n * 2;
        Function<Integer, Integer> addOne = n -> n + 1;
        int result = doubleIt.andThen(addOne).apply(5);
        if (result == 11) { System.out.println("PASS: function compose"); pass++; }
        else System.out.println("FAIL: compose=" + result);

        // 6. Optional
        Optional<String> opt = Optional.of("hello");
        String mapped = opt.map(s -> s.toUpperCase()).orElse("none");
        if ("HELLO".equals(mapped)) { System.out.println("PASS: optional map"); pass++; }
        else System.out.println("FAIL: optional=" + mapped);

        System.out.println(pass + "/6 passed");
    }
}
