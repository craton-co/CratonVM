import java.io.*;
import java.util.*;

// Cross-VM serialization harness for TreeMap / TreeSet.
//   write <file>  : build a known TreeMap+TreeSet, serialize to <file>
//   read  <file>  : deserialize from <file>, print content deterministically
// Run write on one VM, read on the other, diff the read output.
public class TreeSer {
    static TreeMap<String,String> sampleMap() {
        TreeMap<String,String> m = new TreeMap<>();
        m.put("name", "execute");
        m.put("alpha", "1");
        m.put("gamma", "3");
        m.put("beta", "2");
        m.put("zeta", "26");
        return m;
    }
    static TreeMap<Integer,String> sampleIntMap() {
        TreeMap<Integer,String> m = new TreeMap<>();
        for (int i = 10; i >= 1; i--) m.put(i, "v" + i);
        return m;
    }
    static TreeSet<String> sampleSet() {
        TreeSet<String> s = new TreeSet<>();
        s.add("delta"); s.add("apple"); s.add("mango"); s.add("cherry"); s.add("banana");
        return s;
    }
    static TreeSet<Integer> sampleIntSet() {
        TreeSet<Integer> s = new TreeSet<>();
        for (int i = 7; i >= 1; i--) s.add(i * 3);
        return s;
    }

    public static void main(String[] a) throws Exception {
        String mode = a[0], file = a[1];
        if (mode.equals("write")) {
            try (ObjectOutputStream o = new ObjectOutputStream(new FileOutputStream(file))) {
                o.writeObject(sampleMap());
                o.writeObject(sampleIntMap());
                o.writeObject(sampleSet());
                o.writeObject(sampleIntSet());
            }
            System.out.println("WROTE " + file);
        } else {
            try (ObjectInputStream in = new ObjectInputStream(new FileInputStream(file))) {
                @SuppressWarnings("unchecked") TreeMap<String,String> m = (TreeMap<String,String>) in.readObject();
                @SuppressWarnings("unchecked") TreeMap<Integer,String> mi = (TreeMap<Integer,String>) in.readObject();
                @SuppressWarnings("unchecked") TreeSet<String> s = (TreeSet<String>) in.readObject();
                @SuppressWarnings("unchecked") TreeSet<Integer> si = (TreeSet<Integer>) in.readObject();
                System.out.println("MAP    size=" + m.size()  + " " + m);
                System.out.println("INTMAP size=" + mi.size() + " " + mi);
                System.out.println("SET    size=" + s.size()  + " " + s);
                System.out.println("INTSET size=" + si.size() + " " + si);
                // Exercise lookups to ensure the read path (not just toString) is correct.
                System.out.println("get(name)=" + m.get("name") + " get(beta)=" + m.get("beta"));
                System.out.println("intGet(5)=" + mi.get(5) + " contains(mango)=" + s.contains("mango")
                        + " intContains(9)=" + si.contains(9));
                System.out.println("firstKey=" + m.firstKey() + " lastKey=" + m.lastKey()
                        + " setFirst=" + s.first() + " setLast=" + s.last());
            }
        }
    }
}
