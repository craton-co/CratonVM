import java.util.*;

/** `KafkaStreamsConfiguration.asProperties()` is putAll + clone. */
public class PropertiesCloneProbe {
    static int fails = 0;
    static void chk(String what, boolean got) {
        if (!got) fails++;
        System.out.println((got ? "OK   " : "FAIL ") + what);
    }
    static void dump(String tag, Properties p) {
        System.out.println(tag + " cls=" + p.getClass().getName() + " size=" + p.size()
            + " keys=" + new TreeSet<>(toStrings(p.keySet()))
            + " get(bootstrap.servers)=" + p.get("bootstrap.servers"));
    }
    static List<String> toStrings(Set<?> s) {
        List<String> out = new ArrayList<>();
        for (Object o : s) out.add(String.valueOf(o));
        return out;
    }
    public static void main(String[] a) {
        Map<String, Object> configs = new LinkedHashMap<>();
        configs.put("bootstrap.servers", new ArrayList<>(List.of("localhost:9092", "localhost:9093")));
        configs.put("application.id", "my-test-app");

        Properties p = new Properties();
        p.putAll(configs);
        dump("after putAll", p);
        chk("putAll kept both", p.size() == 2 && p.containsKey("bootstrap.servers"));

        Properties c = (Properties) p.clone();
        dump("after clone", c);
        chk("clone kept both", c.size() == 2 && c.containsKey("bootstrap.servers"));
        chk("clone value is the List", c.get("bootstrap.servers") instanceof List);

        // clone of a Properties whose entries were set with setProperty (all String)
        Properties s = new Properties();
        s.setProperty("a", "1");
        s.setProperty("b", "2");
        Properties sc = (Properties) s.clone();
        System.out.println("string-only clone size=" + sc.size() + " a=" + sc.getProperty("a"));
        chk("string-only clone kept both", sc.size() == 2);

        // Hashtable.clone for comparison
        Hashtable<String, Object> h = new Hashtable<>();
        h.put("bootstrap.servers", new ArrayList<>(List.of("x")));
        h.put("application.id", "app");
        @SuppressWarnings("unchecked")
        Hashtable<String, Object> hc = (Hashtable<String, Object>) h.clone();
        System.out.println("hashtable clone size=" + hc.size() + " keys=" + hc.keySet());
        chk("hashtable clone kept both", hc.size() == 2);

        // and putAll from a plain HashMap, for the LinkedHashMap contrast
        Map<String, Object> hm = new HashMap<>();
        hm.put("bootstrap.servers", new ArrayList<>(List.of("y")));
        hm.put("application.id", "app");
        Properties p2 = new Properties();
        p2.putAll(hm);
        Properties p2c = (Properties) p2.clone();
        System.out.println("from HashMap: putAll size=" + p2.size() + " clone size=" + p2c.size());
        chk("HashMap source clone kept both", p2c.size() == 2);

        System.out.println(fails == 0 ? "PROBE-OK" : "PROBE-FAIL " + fails);
    }
}
