public class SimplePair {
    Object first;
    Object second;

    public SimplePair(Object a, Object b) {
        this.first = a;
        this.second = b;
    }

    public static void main(String[] args) {
        SimplePair p = new SimplePair("hello", "world");
        System.out.println("first=" + p.first);
        System.out.println("second=" + p.second);
        System.out.println("DONE");
    }
}
