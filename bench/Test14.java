import java.util.*;

public class Test14 {
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

    public static void main(String[] args) {
        int passed = 0;
        int total = 0;

        total++;
        if (fibonacci(10) == 55) {
            System.out.println("PASS: fib");
            passed++;
        }

        total++;
        String rev = new StringBuilder("hello").reverse().toString();
        if ("olleh".equals(rev)) {
            System.out.println("PASS: reverse");
            passed++;
        }

        total++;
        List<Integer> nums = new ArrayList<>();
        nums.add(1); nums.add(2); nums.add(3);
        int sum = 0;
        for (int x : nums) sum += x;
        if (sum == 6) {
            System.out.println("PASS: list sum");
            passed++;
        }

        total++;
        Map<String, Integer> wc = new HashMap<>();
        wc.put("a", 3);
        if (wc.get("a") == 3) {
            System.out.println("PASS: map");
            passed++;
        }

        total++;
        try {
            int x = 10 / 0;
        } catch (ArithmeticException e) {
            System.out.println("PASS: exception");
            passed++;
        }

        total++;
        if (Math.abs(Math.sqrt(144) - 12.0) < 0.001) {
            System.out.println("PASS: math");
            passed++;
        }

        System.out.println(passed + "/" + total + " passed");
    }
}
