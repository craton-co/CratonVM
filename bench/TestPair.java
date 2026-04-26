public class TestPair {
    static class Pair<A, B> {
        A first; B second;
        Pair(A a, B b) { first = a; second = b; }
        A getFirst() { return first; }
        B getSecond() { return second; }
    }

    public static void main(String[] args) {
        Pair<String, Integer> p = new Pair<>("hello", 42);
        System.out.println("first=" + p.getFirst());
        System.out.println("second=" + p.getSecond());

        Object s = p.getSecond();
        System.out.println("type=" + s.getClass().getName());
        System.out.println("equals42=" + s.equals(42));

        // Direct field access
        System.out.println("field=" + p.second);

        System.out.println("DONE");
    }
}
