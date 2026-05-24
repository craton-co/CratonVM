import java.util.HashMap;

public class HMProbe {
    static class Key {
        final String s;
        Key(String s) { this.s = s; }
        @Override public int hashCode() { return s.hashCode(); }
        @Override public boolean equals(Object o) { return o instanceof Key && ((Key)o).s.equals(s); }
        @Override public String toString() { return "K[" + s + "]"; }
    }
    public static void main(String[] a) {
        Key k1 = new Key("java.lang.Object");
        Key k2 = new Key("java.lang.Object");
        System.out.println("k1.hashCode=" + k1.hashCode());
        System.out.println("k2.hashCode=" + k2.hashCode());
        System.out.println("k1.equals(k2)=" + k1.equals(k2));
        HashMap<Key,String> m = new HashMap<>();
        m.put(k1, "v");
        System.out.println("get(k1)=" + m.get(k1));
        System.out.println("get(k2)=" + m.get(k2));
    }
}
