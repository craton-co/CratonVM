package cratonvm;

/**
 * Session 33: JIT Inline Caching tests.
 *
 * Tests monomorphic, polymorphic, and megamorphic call sites,
 * virtual/interface dispatch, cache miss recovery, hot loops,
 * getter/setter inlining, concurrent dispatch, and deep hierarchies.
 */
public class InlineCacheTest {

    // === Abstract base and concrete subclasses for virtual dispatch ===

    static abstract class Shape {
        abstract int area();
    }

    static class Circle extends Shape {
        int area() { return 10; }
    }

    static class Square extends Shape {
        int area() { return 20; }
    }

    static class Triangle extends Shape {
        int area() { return 30; }
    }

    static class Pentagon extends Shape {
        int area() { return 40; }
    }

    static class Hexagon extends Shape {
        int area() { return 50; }
    }

    static class Heptagon extends Shape {
        int area() { return 60; }
    }

    // === Interface for interface dispatch tests ===

    interface Computable {
        int compute(int x);
    }

    static class Doubler implements Computable {
        public int compute(int x) { return x * 2; }
    }

    static class Tripler implements Computable {
        public int compute(int x) { return x * 3; }
    }

    // === Deep hierarchy for test 10 ===

    static abstract class Animal {
        abstract int legs();
    }

    static abstract class Mammal extends Animal {
        int legs() { return 4; }
        abstract int sound();
    }

    static class Dog extends Mammal {
        int sound() { return 1; }
    }

    static class Cat extends Mammal {
        int sound() { return 2; }
    }

    // === Getter/setter class ===

    static class Box {
        private int value;
        int getValue() { return value; }
        void setValue(int v) { value = v; }
    }

    // ---------------------------------------------------------------
    // Test 1: Monomorphic call site - single receiver type
    // ---------------------------------------------------------------
    public static int testMonomorphicCallSite() {
        Shape s = new Circle();
        int sum = 0;
        for (int i = 0; i < 100; i++) {
            sum += s.area();
        }
        return sum == 1000 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 2: Polymorphic call site - 2-3 receiver types
    // ---------------------------------------------------------------
    public static int testPolymorphicCallSite() {
        Shape[] shapes = new Shape[3];
        shapes[0] = new Circle();
        shapes[1] = new Square();
        shapes[2] = new Triangle();
        int sum = 0;
        for (int i = 0; i < 3; i++) {
            sum += shapes[i].area();
        }
        // 10 + 20 + 30 = 60
        return sum == 60 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 3: Megamorphic call site - many receiver types (>5)
    // ---------------------------------------------------------------
    public static int testMegamorphicCallSite() {
        Shape[] shapes = new Shape[6];
        shapes[0] = new Circle();
        shapes[1] = new Square();
        shapes[2] = new Triangle();
        shapes[3] = new Pentagon();
        shapes[4] = new Hexagon();
        shapes[5] = new Heptagon();
        int sum = 0;
        for (int i = 0; i < 6; i++) {
            sum += shapes[i].area();
        }
        // 10 + 20 + 30 + 40 + 50 + 60 = 210
        return sum == 210 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 4: Virtual dispatch correctness - single impl behind abstract
    // ---------------------------------------------------------------
    public static int testVirtualDispatchCorrectness() {
        Shape s = new Square();
        int result = s.area();
        return result == 20 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 5: Interface dispatch - concrete type through interface ref
    // ---------------------------------------------------------------
    public static int testInterfaceDispatch() {
        Computable c = new Doubler();
        int result = c.compute(21);
        return result == 42 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 6: Cache miss recovery - switch between receiver types
    // ---------------------------------------------------------------
    public static int testCacheMissRecovery() {
        Shape s;
        int sum = 0;
        // Alternate between Circle and Square to force cache misses
        for (int i = 0; i < 20; i++) {
            if (i % 2 == 0) {
                s = new Circle();
            } else {
                s = new Square();
            }
            sum += s.area();
        }
        // 10 circles * 10 + 10 squares * 20 = 100 + 200 = 300
        return sum == 300 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 7: Hot loop with monomorphic receiver - triggers JIT
    // ---------------------------------------------------------------
    public static int testHotLoopMonomorphic() {
        Computable c = new Tripler();
        int sum = 0;
        for (int i = 0; i < 1000; i++) {
            sum += c.compute(1);
        }
        return sum == 3000 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 8: Inlined getter/setter methods
    // ---------------------------------------------------------------
    public static int testInlinedGetterSetter() {
        Box b = new Box();
        b.setValue(42);
        int v = b.getValue();
        return v == 42 ? 1 : 0;
    }

    // ---------------------------------------------------------------
    // Test 9: Concurrent dispatch - two threads calling same virtual
    // ---------------------------------------------------------------
    static volatile int threadResult1 = 0;
    static volatile int threadResult2 = 0;

    public static int testConcurrentDispatch() {
        threadResult1 = 0;
        threadResult2 = 0;
        final Shape s1 = new Circle();
        final Shape s2 = new Square();

        Thread t1 = new Thread(new Runnable() {
            public void run() {
                int sum = 0;
                for (int i = 0; i < 100; i++) {
                    sum += s1.area();
                }
                threadResult1 = sum;
            }
        });
        Thread t2 = new Thread(new Runnable() {
            public void run() {
                int sum = 0;
                for (int i = 0; i < 100; i++) {
                    sum += s2.area();
                }
                threadResult2 = sum;
            }
        });

        t1.start();
        t2.start();
        try { t1.join(); } catch (Exception e) { return 0; }
        try { t2.join(); } catch (Exception e) { return 0; }

        // t1: 100 * 10 = 1000, t2: 100 * 20 = 2000
        if (threadResult1 == 1000 && threadResult2 == 2000) {
            return 1;
        }
        return 0;
    }

    // ---------------------------------------------------------------
    // Test 10: Deep hierarchy - 3+ level virtual dispatch
    // ---------------------------------------------------------------
    public static int testDeepHierarchy() {
        Animal dog = new Dog();
        Animal cat = new Cat();

        int dogLegs = dog.legs();
        int catLegs = cat.legs();

        // Both inherit Mammal.legs() returning 4
        // Dog.sound() = 1, Cat.sound() = 2
        int dogSound = ((Mammal) dog).sound();
        int catSound = ((Mammal) cat).sound();

        // 4 + 4 + 1 + 2 = 11
        int total = dogLegs + catLegs + dogSound + catSound;
        return total == 11 ? 1 : 0;
    }
}
