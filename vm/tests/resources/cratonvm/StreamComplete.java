// JAVA21+
package cratonvm;

import java.util.List;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Map;
import java.util.Set;
import java.util.Optional;
import java.util.stream.Stream;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

/**
 * Session 20: Full java.util.stream Support.
 * Real Streams pipeline execution: filter, map, collect, reduce, etc.
 */
public class StreamComplete {

    // ---- Test 1: Basic filter().map().collect(toList()) pipeline ----

    public static int testFilterMapCollectToList() {
        List<String> names = new ArrayList<>();
        names.add("Alice");
        names.add("Bob");
        names.add("Charlie");
        names.add("Dave");
        names.add("Eve");

        List<String> result = names.stream()
            .filter(n -> n.length() > 3)
            .map(n -> n.toUpperCase())
            .collect(Collectors.toList());

        // Alice(5), Charlie(7), Dave(4) pass filter; Eve(3) and Bob(3) don't
        // Result: [ALICE, CHARLIE, DAVE]
        return result.size(); // 3
    }

    // ---- Test 2: Stream.of() + count() ----

    public static int testStreamOfCount() {
        long count = Stream.of("a", "b", "c", "d", "e").count();
        return (int) count; // 5
    }

    // ---- Test 3: reduce() with identity ----

    public static int testReduceWithIdentity() {
        List<Integer> nums = new ArrayList<>();
        nums.add(1);
        nums.add(2);
        nums.add(3);
        nums.add(4);
        nums.add(5);

        int sum = nums.stream()
            .reduce(0, (a, b) -> a + b);
        return sum; // 15
    }

    // ---- Test 3b: generic reduce(U, BiFunction, BinaryOperator) ----

    public static int testReduceWithGenericAccumulator() {
        int sum = Stream.of(1, 2, 3)
            .reduce(10, (total, value) -> total + value, (left, right) -> left + right);
        return sum == 16 ? 1 : 0;
    }

    // ---- Test 4: forEach() ----

    static int forEachSum = 0;

    public static int testForEach() {
        forEachSum = 0;
        List<Integer> nums = new ArrayList<>();
        nums.add(10);
        nums.add(20);
        nums.add(30);

        nums.stream().forEach(n -> forEachSum += n);
        return forEachSum; // 60
    }

    // ---- Test 5: collect(toSet()) ----

    public static int testCollectToSet() {
        List<String> items = new ArrayList<>();
        items.add("apple");
        items.add("banana");
        items.add("apple"); // duplicate
        items.add("cherry");

        Set<String> result = items.stream().collect(Collectors.toSet());
        return result.size(); // 3 (duplicates removed)
    }

    // ---- Test 6: findFirst() ----

    public static int testFindFirst() {
        List<Integer> nums = new ArrayList<>();
        nums.add(5);
        nums.add(10);
        nums.add(15);

        Optional<Integer> first = nums.stream()
            .filter(n -> n > 7)
            .findFirst();

        return first.isPresent() ? first.get() : -1; // 10
    }

    // ---- Test 7: anyMatch() ----

    public static int testAnyMatch() {
        List<Integer> nums = new ArrayList<>();
        nums.add(1);
        nums.add(2);
        nums.add(3);

        boolean hasEven = nums.stream().anyMatch(n -> n % 2 == 0);
        return hasEven ? 1 : 0; // 1
    }

    // ---- Test 8: allMatch() ----

    public static int testAllMatch() {
        List<Integer> nums = new ArrayList<>();
        nums.add(2);
        nums.add(4);
        nums.add(6);

        boolean allEven = nums.stream().allMatch(n -> n % 2 == 0);
        return allEven ? 1 : 0; // 1
    }

    // ---- Test 9: noneMatch() ----

    public static int testNoneMatch() {
        List<Integer> nums = new ArrayList<>();
        nums.add(1);
        nums.add(3);
        nums.add(5);

        boolean noneEven = nums.stream().noneMatch(n -> n % 2 == 0);
        return noneEven ? 1 : 0; // 1
    }

    // ---- Test 10: sorted() ----

    public static int testSorted() {
        List<Integer> nums = new ArrayList<>();
        nums.add(3);
        nums.add(1);
        nums.add(4);
        nums.add(1);
        nums.add(5);

        List<Integer> sorted = nums.stream()
            .sorted()
            .collect(Collectors.toList());

        // Should be [1, 1, 3, 4, 5]
        return sorted.get(0) * 1000 + sorted.get(1) * 100 + sorted.get(2) * 10 + sorted.get(3); // 1134
    }

    // ---- Test 11: distinct() ----

    public static int testDistinct() {
        List<Integer> nums = new ArrayList<>();
        nums.add(1);
        nums.add(2);
        nums.add(2);
        nums.add(3);
        nums.add(3);
        nums.add(3);

        long count = nums.stream().distinct().count();
        return (int) count; // 3
    }

    // ---- Test 12: limit() and skip() ----

    public static int testLimitSkip() {
        List<Integer> nums = new ArrayList<>();
        for (int i = 1; i <= 10; i++) nums.add(i);

        List<Integer> result = nums.stream()
            .skip(2)      // skip 1, 2
            .limit(3)     // take 3, 4, 5
            .collect(Collectors.toList());

        int sum = 0;
        for (Integer n : result) sum += n;
        return sum; // 3 + 4 + 5 = 12
    }

    // ---- Test 13: toArray() ----

    public static int testToArray() {
        List<String> items = new ArrayList<>();
        items.add("x");
        items.add("y");
        items.add("z");

        Object[] arr = items.stream().toArray();
        return arr.length; // 3
    }

    // ---- Test 14: flatMap() ----

    public static int testFlatMap() {
        List<List<Integer>> nested = new ArrayList<>();
        List<Integer> a = new ArrayList<>();
        a.add(1); a.add(2);
        List<Integer> b = new ArrayList<>();
        b.add(3); b.add(4);
        nested.add(a);
        nested.add(b);

        List<Integer> flat = nested.stream()
            .flatMap(list -> list.stream())
            .collect(Collectors.toList());

        int sum = 0;
        for (Integer n : flat) sum += n;
        return sum; // 1+2+3+4 = 10
    }

    // ---- Test 15: Collectors.joining() ----

    public static int testCollectorsJoining() {
        List<String> words = new ArrayList<>();
        words.add("hello");
        words.add("world");

        String result = words.stream().collect(Collectors.joining(", "));
        return result.equals("hello, world") ? 1 : 0; // 1
    }

    // ---- Test 16: Collectors.toMap() ----

    public static int testCollectorsToMap() {
        List<String> words = new ArrayList<>();
        words.add("one");
        words.add("two");
        words.add("three");

        Map<String, Integer> result = words.stream()
            .collect(Collectors.toMap(w -> w, w -> w.length()));

        return result.get("three"); // 5
    }

    // ---- Test 17: Collectors.groupingBy() ----

    public static int testCollectorsGroupingBy() {
        List<String> words = new ArrayList<>();
        words.add("hi");
        words.add("hey");
        words.add("yo");
        words.add("wow");
        words.add("ok");

        Map<Integer, List<String>> grouped = words.stream()
            .collect(Collectors.groupingBy(w -> w.length()));

        // Length 2: [hi, yo, ok] = 3, Length 3: [hey, wow] = 2
        return grouped.get(2).size() * 10 + grouped.get(3).size(); // 32
    }

    // ---- Test 18: Stream.empty() ----

    public static int testStreamEmpty() {
        long count = Stream.empty().count();
        return (int) count; // 0
    }

    // ---- Test 19: reduce() returning Optional ----

    public static int testReduceOptional() {
        List<Integer> nums = new ArrayList<>();
        nums.add(5);
        nums.add(10);
        nums.add(3);

        Optional<Integer> max = nums.stream()
            .reduce((a, b) -> a > b ? a : b);

        return max.isPresent() ? max.get() : -1; // 10
    }

    // ---- Test 20: Collectors.counting() with groupingBy ----

    public static int testGroupingByCounting() {
        List<String> items = new ArrayList<>();
        items.add("a");
        items.add("b");
        items.add("a");
        items.add("c");
        items.add("b");
        items.add("a");

        Map<String, Long> counts = items.stream()
            .collect(Collectors.groupingBy(x -> x, Collectors.counting()));

        return counts.get("a").intValue(); // 3
    }

    // ---- Test 21: Stream.concat() ----

    public static int testStreamConcat() {
        Stream<String> s1 = Stream.of("a", "b");
        Stream<String> s2 = Stream.of("c", "d");

        long count = Stream.concat(s1, s2).count();
        return (int) count; // 4
    }

    // ---- Test 22: Stream.toList() (Java 16+) ----

    public static int testStreamToList() {
        List<String> items = new ArrayList<>();
        items.add("x");
        items.add("y");

        List<String> result = items.stream()
            .map(s -> s + "!")
            .toList();

        return result.size(); // 2
    }

    // ---- Test 23: min() and max() ----

    public static int testMinMax() {
        List<Integer> nums = new ArrayList<>();
        nums.add(3);
        nums.add(1);
        nums.add(4);
        nums.add(1);
        nums.add(5);

        Optional<Integer> min = nums.stream().min((a, b) -> a - b);
        Optional<Integer> max = nums.stream().max((a, b) -> a - b);

        int minVal = min.isPresent() ? min.get() : -1;
        int maxVal = max.isPresent() ? max.get() : -1;
        return minVal * 10 + maxVal; // 15
    }

    // ---- Test 24: peek() ----

    static int peekCount = 0;

    public static int testPeek() {
        peekCount = 0;
        List<Integer> nums = new ArrayList<>();
        nums.add(1);
        nums.add(2);
        nums.add(3);

        long count = nums.stream()
            .peek(n -> peekCount++)
            .count();

        return peekCount * 10 + (int) count; // 33
    }

    // ---- Test 25: Chained pipeline: filter + map + sorted + limit + collect ----

    public static int testChainedPipeline() {
        List<Integer> nums = new ArrayList<>();
        for (int i = 1; i <= 20; i++) nums.add(i);

        List<Integer> result = nums.stream()
            .filter(n -> n % 2 == 0)     // 2,4,6,8,10,12,14,16,18,20
            .map(n -> n * n)             // 4,16,36,64,100,144,196,256,324,400
            .sorted()                     // already sorted
            .limit(3)                     // 4, 16, 36
            .collect(Collectors.toList());

        int sum = 0;
        for (Integer n : result) sum += n;
        return sum; // 4 + 16 + 36 = 56
    }

    // ---- Test 26: Collectors.partitioningBy() ----

    public static int testPartitioningBy() {
        List<Integer> nums = new ArrayList<>();
        nums.add(1); nums.add(2); nums.add(3); nums.add(4); nums.add(5);

        Map<Boolean, List<Integer>> parts = nums.stream()
            .collect(Collectors.partitioningBy(n -> n % 2 == 0));

        int evens = parts.get(true).size();
        int odds = parts.get(false).size();
        return evens * 10 + odds; // 23
    }

    // ---- Test 27: IntStream.range/sum ----

    public static int testIntStreamRangeSum() {
        int sum = IntStream.range(1, 6).sum(); // 1+2+3+4+5
        return sum; // 15
    }

    // ---- Test 28: mapToInt + sum ----

    public static int testMapToIntSum() {
        List<String> words = new ArrayList<>();
        words.add("hi");
        words.add("hey");
        words.add("hello");

        int totalLength = words.stream()
            .mapToInt(w -> w.length())
            .sum();
        return totalLength; // 2+3+5 = 10
    }

    // ---- Test 29: Stream.of single element ----

    public static int testStreamOfSingle() {
        Optional<String> result = Stream.of("only")
            .findFirst();
        return result.isPresent() && result.get().equals("only") ? 1 : 0; // 1
    }

    // ---- Test 30: Parallel stream (sequential behavior ok) ----

    public static int testParallelStream() {
        List<Integer> nums = new ArrayList<>();
        for (int i = 1; i <= 100; i++) nums.add(i);

        int sum = nums.stream()
            .parallel()
            .reduce(0, (a, b) -> a + b);
        return sum == 5050 ? 1 : 0; // 1
    }
}
