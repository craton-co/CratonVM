import java.util.ArrayList;
import java.util.HashMap;

/**
 * Comprehensive benchmark suite for RustJVM vs OpenJDK comparison.
 * All benchmarks use System.currentTimeMillis() for portable timing.
 * Compiled with: javac --release 8 Benchmark.java
 */
public class Benchmark {

    // ========================================================================
    // 1. Arithmetic: tight loop with integer math
    // ========================================================================
    static long benchArithmetic(int iterations) {
        long sum = 0;
        for (int i = 0; i < iterations; i++) {
            sum += i * 3 - i / 2 + i % 7;
        }
        return sum;
    }

    // ========================================================================
    // 2. Fibonacci (recursive) — measures call overhead + stack frames
    // ========================================================================
    static int fib(int n) {
        if (n <= 1) return n;
        return fib(n - 1) + fib(n - 2);
    }

    // ========================================================================
    // 3. String concatenation via StringBuilder
    // ========================================================================
    static String benchStringBuilder(int iterations) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < iterations; i++) {
            sb.append("item");
            sb.append(i);
            sb.append(",");
        }
        return sb.toString();
    }

    // ========================================================================
    // 4. ArrayList: add + get + iterate
    // ========================================================================
    static long benchArrayList(int size) {
        ArrayList<Integer> list = new ArrayList<>();
        for (int i = 0; i < size; i++) {
            list.add(i);
        }
        long sum = 0;
        for (int i = 0; i < list.size(); i++) {
            sum += list.get(i);
        }
        return sum;
    }

    // ========================================================================
    // 5. HashMap: put + get
    // ========================================================================
    static long benchHashMap(int size) {
        HashMap<String, Integer> map = new HashMap<>();
        for (int i = 0; i < size; i++) {
            map.put("key" + i, i);
        }
        long sum = 0;
        for (int i = 0; i < size; i++) {
            Integer v = map.get("key" + i);
            if (v != null) sum += v;
        }
        return sum;
    }

    // ========================================================================
    // 6. Object allocation + virtual dispatch
    // ========================================================================
    static abstract class Animal {
        abstract String speak();
    }
    static class Dog extends Animal {
        String speak() { return "woof"; }
    }
    static class Cat extends Animal {
        String speak() { return "meow"; }
    }
    static class Bird extends Animal {
        String speak() { return "tweet"; }
    }

    static int benchPolymorphism(int iterations) {
        Animal[] animals = new Animal[] { new Dog(), new Cat(), new Bird() };
        int count = 0;
        for (int i = 0; i < iterations; i++) {
            Animal a = animals[i % 3];
            if (a.speak().length() > 0) {
                count++;
            }
        }
        return count;
    }

    // ========================================================================
    // 7. Exception handling: try/catch in a loop
    // ========================================================================
    static int benchExceptions(int iterations) {
        int caught = 0;
        for (int i = 0; i < iterations; i++) {
            try {
                if (i % 10 == 0) {
                    throw new RuntimeException("test");
                }
            } catch (RuntimeException e) {
                caught++;
            }
        }
        return caught;
    }

    // ========================================================================
    // 8. Sieve of Eratosthenes — array-heavy computation
    // ========================================================================
    static int sieve(int limit) {
        boolean[] composite = new boolean[limit + 1];
        int count = 0;
        for (int i = 2; i <= limit; i++) {
            if (!composite[i]) {
                count++;
                for (int j = i + i; j <= limit; j += i) {
                    composite[j] = true;
                }
            }
        }
        return count;
    }

    // ========================================================================
    // 9. Matrix multiplication (nested loops + array access)
    // ========================================================================
    static int[][] matmul(int[][] a, int[][] b, int n) {
        int[][] c = new int[n][n];
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                int sum = 0;
                for (int k = 0; k < n; k++) {
                    sum += a[i][k] * b[k][j];
                }
                c[i][j] = sum;
            }
        }
        return c;
    }

    // ========================================================================
    // 10. Bubble sort — array swaps + comparisons
    // ========================================================================
    static void bubbleSort(int[] arr) {
        int n = arr.length;
        for (int i = 0; i < n - 1; i++) {
            for (int j = 0; j < n - i - 1; j++) {
                if (arr[j] > arr[j + 1]) {
                    int tmp = arr[j];
                    arr[j] = arr[j + 1];
                    arr[j + 1] = tmp;
                }
            }
        }
    }

    // ========================================================================
    // Runner
    // ========================================================================
    public static void main(String[] args) {
        System.out.println("=== RustJVM Performance Benchmark ===");
        System.out.println();

        long total = 0;

        // 1. Arithmetic (1M iterations)
        {
            long t0 = System.currentTimeMillis();
            long r = benchArithmetic(1000000);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("1. Arithmetic (1M ops)    : " + elapsed + " ms  [checksum=" + r + "]");
        }

        // 2. Fibonacci(28)
        {
            long t0 = System.currentTimeMillis();
            int r = fib(28);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("2. Fibonacci(28)          : " + elapsed + " ms  [result=" + r + "]");
        }

        // 3. StringBuilder (5K appends)
        {
            long t0 = System.currentTimeMillis();
            String r = benchStringBuilder(5000);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("3. StringBuilder (5K)     : " + elapsed + " ms  [len=" + r.length() + "]");
        }

        // 4. ArrayList (50K elements)
        {
            long t0 = System.currentTimeMillis();
            long r = benchArrayList(50000);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("4. ArrayList (50K)        : " + elapsed + " ms  [sum=" + r + "]");
        }

        // 5. HashMap (10K entries)
        {
            long t0 = System.currentTimeMillis();
            long r = benchHashMap(10000);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("5. HashMap (10K)          : " + elapsed + " ms  [sum=" + r + "]");
        }

        // 6. Polymorphism (500K dispatches)
        {
            long t0 = System.currentTimeMillis();
            int r = benchPolymorphism(500000);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("6. Polymorphism (500K)    : " + elapsed + " ms  [count=" + r + "]");
        }

        // 7. Exceptions (10K iterations)
        {
            long t0 = System.currentTimeMillis();
            int r = benchExceptions(10000);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("7. Exceptions (10K)       : " + elapsed + " ms  [caught=" + r + "]");
        }

        // 8. Sieve of Eratosthenes (100K)
        {
            long t0 = System.currentTimeMillis();
            int r = sieve(100000);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("8. Sieve (100K)           : " + elapsed + " ms  [primes=" + r + "]");
        }

        // 9. Matrix multiplication (100x100)
        {
            int n = 100;
            int[][] a = new int[n][n];
            int[][] b = new int[n][n];
            for (int i = 0; i < n; i++) {
                for (int j = 0; j < n; j++) {
                    a[i][j] = i + j;
                    b[i][j] = i - j;
                }
            }
            long t0 = System.currentTimeMillis();
            int[][] c = matmul(a, b, n);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("9. Matrix 100x100         : " + elapsed + " ms  [c[50][50]=" + c[50][50] + "]");
        }

        // 10. Bubble sort (3000 elements)
        {
            int[] arr = new int[3000];
            for (int i = 0; i < arr.length; i++) {
                arr[i] = arr.length - i;
            }
            long t0 = System.currentTimeMillis();
            bubbleSort(arr);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("10. Bubble sort (3K)      : " + elapsed + " ms  [first=" + arr[0] + "]");
        }

        System.out.println();
        System.out.println("TOTAL                     : " + total + " ms");
    }
}
