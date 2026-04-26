public class GenPair<A, B> {
    A first;
    B second;

    public GenPair(A a, B b) {
        this.first = a;
        this.second = b;
    }

    public A getFirst() { return first; }
    public B getSecond() { return second; }

    public static void main(String[] args) {
        GenPair<String, Integer> p = new GenPair<>("hello", 42);
        System.out.println("first=" + p.getFirst());
        System.out.println("second=" + p.getSecond());
        System.out.println("DONE");
    }
}
