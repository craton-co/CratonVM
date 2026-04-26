public class GenPair2 {
    Object first;
    Object second;

    public GenPair2(Object a, Object b) {
        this.first = a;
        this.second = b;
    }

    public Object getFirst() { return first; }

    public static void main(String[] args) {
        GenPair2 p = new GenPair2("hello", "world");
        System.out.println("first=" + p.getFirst());
        System.out.println("field=" + p.first);
        System.out.println("DONE");
    }
}
