import java.util.ArrayList;

public class Demo {
    public static void main(String[] args) {
        System.out.println("=== RustJVM Multi-Class Demo ===");
        System.out.println();

        // --- Point class ---
        System.out.println("--- Points ---");
        Point a = new Point(3, 4);
        Point b = new Point(6, 8);
        System.out.println("A = " + a);
        System.out.println("B = " + b);
        System.out.println("A + B = " + a.add(b));
        System.out.println("Distance A->B = " + a.distanceTo(b));
        System.out.println();

        // --- Inheritance / polymorphism ---
        System.out.println("--- Shapes (inheritance) ---");
        Shape circle = new Circle(5.0);
        Shape rect = new Rectangle(4.0, 7.0);
        System.out.println(circle);
        System.out.println(rect);
        System.out.println();

        // --- ArrayList ---
        System.out.println("--- ArrayList ---");
        ArrayList<String> names = new ArrayList<>();
        names.add("Alice");
        names.add("Bob");
        names.add("Charlie");
        System.out.println("Size: " + names.size());
        for (int i = 0; i < names.size(); i++) {
            System.out.println("  [" + i + "] " + names.get(i));
        }
        System.out.println();

        // --- Exception handling ---
        System.out.println("--- Exceptions ---");
        try {
            int result = 100 / 0;
            System.out.println("Should not reach here: " + result);
        } catch (ArithmeticException e) {
            System.out.println("Caught: " + e.getMessage());
        }

        try {
            String s = null;
            int len = s.length();
            System.out.println("Should not reach here: " + len);
        } catch (NullPointerException e) {
            System.out.println("Caught NPE: OK");
        }
        System.out.println();

        // --- String operations ---
        System.out.println("--- Strings ---");
        String hello = "Hello";
        String world = "World";
        String combined = hello + ", " + world + "!";
        System.out.println(combined);
        System.out.println("Length: " + combined.length());
        System.out.println("Upper: " + combined.toUpperCase());
        System.out.println("Starts with Hello: " + combined.startsWith("Hello"));
        System.out.println("Index of World: " + combined.indexOf("World"));
        System.out.println();

        // --- Math ---
        System.out.println("--- Math ---");
        System.out.println("PI = " + Math.PI);
        System.out.println("sqrt(144) = " + Math.sqrt(144.0));
        System.out.println("abs(-42) = " + Math.abs(-42));
        System.out.println("max(10, 20) = " + Math.max(10, 20));
        System.out.println("pow(2, 10) = " + Math.pow(2.0, 10.0));
        System.out.println();

        // --- Integer parsing / autoboxing ---
        System.out.println("--- Wrapper types ---");
        int parsed = Integer.parseInt("12345");
        System.out.println("Parsed: " + parsed);
        System.out.println("MAX_INT: " + Integer.MAX_VALUE);
        System.out.println("Hex 255: " + Integer.toHexString(255));
        System.out.println();

        System.out.println("=== All tests passed! ===");
    }
}
