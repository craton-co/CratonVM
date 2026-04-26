import java.util.*;

public class TestGenerics {
    // Generic class
    static class Pair<A, B> {
        A first; B second;
        Pair(A a, B b) { first = a; second = b; }
        A getFirst() { return first; }
        B getSecond() { return second; }
    }

    // Generic method
    static <T extends Comparable<T>> T max(T a, T b) {
        return a.compareTo(b) >= 0 ? a : b;
    }

    // Wildcard
    static double sumList(List<? extends Number> list) {
        double sum = 0;
        for (int i = 0; i < list.size(); i++) {
            sum += list.get(i).doubleValue();
        }
        return sum;
    }

    public static void main(String[] args) {
        int pass = 0;

        // 1. Generic class
        Pair<String, Integer> p = new Pair<>("hello", 42);
        if ("hello".equals(p.getFirst()) && p.getSecond() == 42) {
            System.out.println("PASS: generic pair"); pass++;
        } else System.out.println("FAIL: pair");

        // 2. Generic method
        String bigger = max("apple", "banana");
        if ("banana".equals(bigger)) { System.out.println("PASS: generic max"); pass++; }
        else System.out.println("FAIL: max=" + bigger);

        // 3. Wildcard sum
        List<Integer> ints = new ArrayList<>();
        ints.add(1); ints.add(2); ints.add(3);
        double s = sumList(ints);
        if (Math.abs(s - 6.0) < 0.001) { System.out.println("PASS: wildcard sum"); pass++; }
        else System.out.println("FAIL: sum=" + s);

        // 4. Generic with interface
        List<String> words = new ArrayList<>();
        words.add("banana"); words.add("apple"); words.add("cherry");
        Collections.sort(words);
        if ("apple".equals(words.get(0))) { System.out.println("PASS: generic sort"); pass++; }
        else System.out.println("FAIL: sort=" + words.get(0));

        System.out.println(pass + "/4 passed");
    }
}
