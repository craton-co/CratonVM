import java.io.*;
import java.util.*;
import java.util.concurrent.*;

public class ChmSerProbe {
    static byte[] ser(Object o) throws Exception {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        try (ObjectOutputStream os = new ObjectOutputStream(b)) { os.writeObject(o); }
        return b.toByteArray();
    }
    @SuppressWarnings("unchecked")
    static <T> T deser(byte[] b) throws Exception {
        try (ObjectInputStream is = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return (T) is.readObject();
        }
    }
    static class Holder implements Serializable {
        Map<Object,String> m = new ConcurrentHashMap<>();
        Object key;
    }
    public static void main(String[] args) throws Exception {
        ConcurrentHashMap<String,String> chm = new ConcurrentHashMap<>();
        chm.put("a", "1"); chm.put("b", "2");
        Map<String,String> r1 = deser(ser(chm));
        System.out.println("CHM roundtrip: size=" + r1.size() + " a=" + r1.get("a") + " b=" + r1.get("b") + " cls=" + r1.getClass().getName());

        HashMap<String,String> hm = new HashMap<>();
        hm.put("a", "1"); hm.put("b", "2");
        Map<String,String> r2 = deser(ser(hm));
        System.out.println("HashMap roundtrip: size=" + r2.size() + " a=" + r2.get("a"));

        // identity sharing across one stream + CHM keyed by identity
        Holder h = new Holder();
        Object k = new Object() { private static final long serialVersionUID = 1L; };
        // use a serializable key
        StringBuilder skb = null;
        String key = new String("theKey");
        h.key = key;
        h.m.put(key, "val");
        Holder rh = deser(ser(h));
        System.out.println("Holder: mapSize=" + rh.m.size() + " keySame=" + (rh.m.containsKey(rh.key)) + " get=" + rh.m.get(rh.key));

        ConcurrentHashMap<Object,String> idm = new ConcurrentHashMap<>();
        Object bean = new Bean();
        idm.put(bean, "em");
        Object[] pair = new Object[] { idm, bean };
        Object[] rp = deser(ser(pair));
        ConcurrentHashMap<Object,String> idm2 = (ConcurrentHashMap<Object,String>) rp[0];
        Object bean2 = rp[1];
        System.out.println("identity pair: mapSize=" + idm2.size() + " remove=" + idm2.remove(bean2));

        ConcurrentLinkedQueue<String> clq = new ConcurrentLinkedQueue<>(List.of("x","y"));
        ConcurrentLinkedQueue<String> clq2 = deser(ser(clq));
        System.out.println("CLQ roundtrip: size=" + clq2.size());
        CopyOnWriteArrayList<String> cow = new CopyOnWriteArrayList<>(List.of("x","y"));
        System.out.println("COW roundtrip: size=" + ((CopyOnWriteArrayList<String>) deser(ser(cow))).size());
    }
    static class Bean implements Serializable { int v = 7; }
}
