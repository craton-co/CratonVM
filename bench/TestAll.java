import java.util.*;
import java.util.stream.*;

public class TestAll {
    public static void main(String[] args) {
        int pass = 0;
        int total = 0;

        // 1. Arithmetic
        total++; if (fib(10) == 55) { p("fib"); pass++; } else f("fib=" + fib(10));

        // 2. String reverse
        total++; if ("olleh".equals(new StringBuilder("hello").reverse().toString())) { p("reverse"); pass++; } else f("reverse");

        // 3. ArrayList for-each
        total++; List<Integer> nums = new ArrayList<>();
        nums.add(1); nums.add(2); nums.add(3);
        int sum = 0; for (int x : nums) sum += x;
        if (sum == 6) { p("list-foreach"); pass++; } else f("list=" + sum);

        // 4. HashMap
        total++; Map<String, Integer> m = new HashMap<>();
        m.put("a", 1); m.put("b", 2);
        if (m.get("a") == 1) { p("hashmap"); pass++; } else f("map");

        // 5. Exception
        total++; try { int x = 10/0; f("no-exc"); } catch (ArithmeticException e) { p("exception"); pass++; }

        // 6. Sort with Comparator lambda
        total++; List<String> names = new ArrayList<>();
        names.add("C"); names.add("A"); names.add("B");
        names.sort((a, b) -> a.compareTo(b));
        if ("A".equals(names.get(0))) { p("sort-lambda"); pass++; } else f("sort=" + names.get(0));

        // 7. Stream filter+count
        total++; long ct = names.stream().filter(s -> s.compareTo("B") >= 0).count();
        if (ct == 2) { p("stream-filter"); pass++; } else f("filter=" + ct);

        // 8. Switch
        total++; if ("weekday".equals(dayType(3))) { p("switch"); pass++; } else f("switch");

        // 9. Varargs
        total++; if (vsum(1,2,3,4,5) == 15) { p("varargs"); pass++; } else f("varargs");

        // 10. Math
        total++; if (Math.abs(Math.sqrt(2) - 1.41421356) < 0.001) { p("math"); pass++; } else f("math");

        // 11. String.format
        total++; String fmt = String.format("x=%d y=%s", 42, "hi");
        if ("x=42 y=hi".equals(fmt)) { p("format"); pass++; } else f("fmt=" + fmt);

        // 12. Inner class polymorphism
        total++; Shape c = new Circle(3);
        if (Math.abs(c.area() - Math.PI * 9) < 0.001) { p("polymorphism"); pass++; } else f("poly");

        System.out.println("\n" + pass + "/" + total + " passed");
    }

    static void p(String t) { System.out.println("PASS: " + t); }
    static void f(String t) { System.out.println("FAIL: " + t); }
    static int fib(int n) { int a=0,b=1; for(int i=2;i<=n;i++){int t=a+b;a=b;b=t;} return n<=1?n:b; }
    static String dayType(int d) { switch(d){case 1:case 2:case 3:case 4:case 5:return "weekday";case 6:case 7:return "weekend";default:return "?";} }
    static int vsum(int... ns) { int s=0; for(int n:ns) s+=n; return s; }

    static abstract class Shape { abstract double area(); }
    static class Circle extends Shape {
        double r; Circle(double r){this.r=r;} double area(){return Math.PI*r*r;}
    }
}
