import java.util.AbstractCollection;
import java.util.Iterator;

public class IntListProbe {
    static abstract class MyImmutableCollection<E> extends AbstractCollection<E> {
        public boolean add(E e) { throw new UnsupportedOperationException(); }
    }
    static class MyImmutableList<E> extends MyImmutableCollection<E> {
        private final Object[] data;
        MyImmutableList(Object... d) { data = d; }
        @SuppressWarnings("unchecked")
        public Iterator<E> iterator() {
            return new Iterator<E>() {
                int i = 0;
                public boolean hasNext() { return i < data.length; }
                public E next() { return (E) data[i++]; }
            };
        }
        public int size() { return data.length; }
    }
    public static void main(String[] a) {
        MyImmutableList<Integer> xs = new MyImmutableList<>(1, 2, 3);
        String t = xs.toString();
        System.out.println("t=" + t);
        System.out.println("xs=" + xs);
        System.out.println(xs);
        System.out.println("v=" + String.valueOf(xs));
        System.out.println("OK");
    }
}
