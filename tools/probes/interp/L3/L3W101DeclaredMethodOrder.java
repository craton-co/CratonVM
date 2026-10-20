import java.lang.reflect.*;
import java.util.*;

/**
 * Interpreter round i5 wave 101, lane L3 (proposal
 * i100-L3-proposal-hotspot-method-array-order-for-every-class):
 * {@code getDeclaredMethods()} UNSORTED, as HotSpot 25 lists it (its sorted
 * {@code InstanceKlass::methods()} order, keyed by name-Symbol address).
 *
 * Run with and without CRATONVM_REFLECT_HOTSPOT_METHOD_ORDER=1, both modes.
 * Rows "jdk" must be SAME with the switch on (every name of an archived JDK
 * class is archived, so the model is exact there). Rows "app" mix archived and
 * run-time names; they measure the constant-pool approximation, and HotSpot
 * itself may differ run to run on them only if symbol allocation does.
 * Rows "set" print the sorted contents, which must be SAME either way.
 */
public class L3W101DeclaredMethodOrder {
    enum Color { RED, GREEN; Color next() { return GREEN; } }

    record Point(int x, int y) {
        Point { if (x < 0) throw new IllegalArgumentException(); }
        int sum() { return x + y; }
    }

    static class Animal implements Comparable<Animal> {
        Object make() { return null; }
        public int compareTo(Animal o) { return 0; }
        void feed(int n) { }
        void feed(String s) { }
        void feed(long n, int k) { }
    }

    static class Dog extends Animal {
        @Override Dog make() { return this; } // covariant: bridge make()Object
    }

    // Names both archived (run, get, equals, size) and new to the VM
    // (zebraWalk, alphaJump, quuxFrob); quuxFrob is referenced from code
    // before it is declared, so its pool entry comes before alphaJump's.
    static class App {
        void zebraWalk() { quuxFrob(); }
        void alphaJump() { }
        public void run() { }
        Object get() { return null; }
        public boolean equals(Object o) { return false; }
        int size() { return 0; }
        void quuxFrob() { }
        static void quuxFrob(int n) { }
    }

    static String names(Class<?> c) {
        StringBuilder sb = new StringBuilder();
        for (Method m : c.getDeclaredMethods()) {
            if (sb.length() > 0) sb.append(' ');
            sb.append(m.getName()).append('/').append(m.getParameterCount());
            if (m.isBridge()) sb.append('b');
        }
        return sb.toString();
    }

    // Interpreter round i5 wave 103, lane L3: the names App's class file
    // creates (no earlier class, and not the CDS archive, has them).
    static final Set<String> RUN_TIME_NAMES = Set.of("zebraWalk", "alphaJump", "quuxFrob");

    /**
     * App's row, order-insensitive where HotSpot itself is: a symbol a
     * non-boot loader's class creates is malloc'ed one by one
     * (SymbolTable::allocate_symbol, c_heap; Symbol::operator new ->
     * AllocateHeap), so the relative address order of App's three new names
     * -- the key Method::sort_methods sorts by -- is the allocator's, not the
     * class file's (Windows' low-fragmentation heap randomizes slots within a
     * size class; measured to vary run to run). The quicksort then also orders
     * quuxFrob's two overloads by that path. So: the archived names in array
     * order (their keys are archive offsets, distinct, hence a fixed relative
     * order whatever the run-time keys are), then the run-time names sorted.
     */
    static String appNames(Class<?> c) {
        StringBuilder archived = new StringBuilder();
        List<String> runTime = new ArrayList<>();
        for (Method m : c.getDeclaredMethods()) {
            String id = m.getName() + '/' + m.getParameterCount() + (m.isBridge() ? "b" : "");
            if (RUN_TIME_NAMES.contains(m.getName())) {
                runTime.add(id);
            } else {
                if (archived.length() > 0) archived.append(' ');
                archived.append(id);
            }
        }
        Collections.sort(runTime);
        return archived + " | run-time " + String.join(" ", runTime);
    }

    static String sorted(Class<?> c) {
        List<String> l = new ArrayList<>();
        for (Method m : c.getDeclaredMethods()) l.add(m.toString());
        Collections.sort(l);
        return l.size() + " " + l.hashCode();
    }

    public static void main(String[] args) {
        Class<?>[] jdk = {
            Object.class, String.class, HashMap.class, Thread.class, Integer.class,
            ArrayList.class, Runnable.class, Thread.State.class, StringBuilder.class,
        };
        for (Class<?> c : jdk) {
            System.out.println("jdk " + c.getName() + ": " + names(c));
        }
        Class<?>[] app = { Color.class, Point.class, Animal.class, Dog.class, App.class };
        for (Class<?> c : app) {
            System.out.println("app " + c.getSimpleName() + ": "
                    + (c == App.class ? appNames(c) : names(c)));
        }
        for (Class<?> c : jdk) System.out.println("set " + c.getName() + ": " + sorted(c));
        for (Class<?> c : app) System.out.println("set " + c.getSimpleName() + ": " + sorted(c));
        // getMethods() is built from getDeclaredMethods0(publicOnly=true).
        StringBuilder sb = new StringBuilder();
        for (Method m : Object.class.getMethods()) sb.append(m.getName()).append(' ');
        System.out.println("jdk Object.getMethods: " + sb.toString().trim());
    }
}
