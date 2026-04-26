import java.util.*;
import java.util.stream.*;

public class IntegrationTest {
    // Test 1: Basic arithmetic and control flow
    static int fibonacci(int n) {
        if (n <= 1) return n;
        int a = 0, b = 1;
        for (int i = 2; i <= n; i++) {
            int temp = a + b;
            a = b;
            b = temp;
        }
        return b;
    }

    // Test 2: String operations
    static String reverseString(String s) {
        StringBuilder sb = new StringBuilder(s);
        return sb.reverse().toString();
    }

    // Test 3: Collections
    static int sumList(List<Integer> list) {
        int sum = 0;
        for (int x : list) {
            sum += x;
        }
        return sum;
    }

    // Test 4: HashMap
    static Map<String, Integer> wordCount(String[] words) {
        Map<String, Integer> map = new HashMap<>();
        for (String w : words) {
            map.put(w, map.getOrDefault(w, 0) + 1);
        }
        return map;
    }

    // Test 5: Exceptions
    static String safeDivide(int a, int b) {
        try {
            return String.valueOf(a / b);
        } catch (ArithmeticException e) {
            return "error: " + e.getMessage();
        }
    }

    // Test 6: Inheritance
    static abstract class Shape {
        abstract double area();
    }
    static class Circle extends Shape {
        double radius;
        Circle(double r) { this.radius = r; }
        double area() { return Math.PI * radius * radius; }
    }
    static class Rectangle extends Shape {
        double w, h;
        Rectangle(double w, double h) { this.w = w; this.h = h; }
        double area() { return w * h; }
    }

    // Test 7: Streams
    static long countEven(int[] nums) {
        return Arrays.stream(nums)
            .filter(n -> n % 2 == 0)
            .count();
    }

    public static void main(String[] args) {
        int passed = 0;
        int total = 0;

        // Test 1: Fibonacci
        total++;
        if (fibonacci(10) == 55) {
            System.out.println("PASS: fibonacci(10) = 55");
            passed++;
        } else {
            System.out.println("FAIL: fibonacci(10) = " + fibonacci(10));
        }

        // Test 2: String reverse
        total++;
        if ("olleh".equals(reverseString("hello"))) {
            System.out.println("PASS: reverse(hello) = olleh");
            passed++;
        } else {
            System.out.println("FAIL: reverse(hello) = " + reverseString("hello"));
        }

        // Test 3: List sum
        total++;
        List<Integer> nums = new ArrayList<>();
        nums.add(1); nums.add(2); nums.add(3); nums.add(4); nums.add(5);
        if (sumList(nums) == 15) {
            System.out.println("PASS: sumList = 15");
            passed++;
        } else {
            System.out.println("FAIL: sumList = " + sumList(nums));
        }

        // Test 4: HashMap word count
        total++;
        Map<String, Integer> wc = wordCount(new String[]{"a", "b", "a", "c", "b", "a"});
        if (wc.get("a") == 3) {
            System.out.println("PASS: wordCount(a) = 3");
            passed++;
        } else {
            System.out.println("FAIL: wordCount(a) = " + wc.get("a"));
        }

        // Test 5: Exception handling
        total++;
        if ("error: / by zero".equals(safeDivide(10, 0))) {
            System.out.println("PASS: safeDivide catches ArithmeticException");
            passed++;
        } else {
            System.out.println("FAIL: safeDivide = " + safeDivide(10, 0));
        }

        // Test 6: Polymorphism
        total++;
        Shape circle = new Circle(5);
        Shape rect = new Rectangle(3, 4);
        double totalArea = circle.area() + rect.area();
        if (Math.abs(totalArea - (Math.PI * 25 + 12)) < 0.001) {
            System.out.println("PASS: polymorphic area = " + totalArea);
            passed++;
        } else {
            System.out.println("FAIL: area = " + totalArea);
        }

        // Test 7: Math
        total++;
        if (Math.abs(Math.sqrt(144) - 12.0) < 0.001) {
            System.out.println("PASS: sqrt(144) = 12.0");
            passed++;
        } else {
            System.out.println("FAIL: sqrt(144) = " + Math.sqrt(144));
        }

        System.out.println("\n" + passed + "/" + total + " tests passed");
    }
}
