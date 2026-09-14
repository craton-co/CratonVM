import java.io.InputStream;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Deque;
import java.util.HashMap;
import java.util.HashSet;
import java.util.IdentityHashMap;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.concurrent.ConcurrentHashMap;
import java.util.function.Supplier;

/**
 * After a collection class is redefined, does an instance built BEFORE the
 * redefinition still answer correctly?
 *
 * CratonVM implements many JDK collections as small synthetic objects whose
 * methods are natives. A class redefinition evicts native shadows, and any
 * method not on the forced-native allow-list then runs the real JDK body
 * against a layout the object does not have. That is the same defect already
 * fixed for StringBuilder — see
 * {@code is_string_builder_layout_native_override} in
 * {@code vm/src/runtime/interpreter/invoke.rs}.
 *
 * The redefinition installs each class's OWN bytes, so nothing about the class
 * changes and every difference below is the VM's doing.
 *
 * <pre>
 *   java     RedefineCollectionLayoutProbe   # control: redefine is skipped
 *   cratonvm RedefineCollectionLayoutProbe   # must print PROBE PASS
 * </pre>
 */
public final class RedefineCollectionLayoutProbe {

    private record Case(String name, Supplier<String> op) {}

    private static final List<Case> CASES = new ArrayList<>();

    // All built ONCE, before any redefinition, and reused afterwards.
    private static final Map<Class<?>, String> IDENTITY = new IdentityHashMap<>(16);
    private static final Map<String, String> HASH = new HashMap<>();
    private static final Map<String, String> LINKED = new LinkedHashMap<>();
    private static final Map<String, String> TREE = new TreeMap<>();
    private static final Map<String, String> CHM = new ConcurrentHashMap<>();
    private static final List<String> ALIST = new ArrayList<>();
    private static final List<String> LLIST = new LinkedList<>();
    private static final Set<String> HSET = new HashSet<>();
    private static final Set<String> LSET = new LinkedHashSet<>();
    private static final Set<String> TSET = new TreeSet<>();
    private static final Deque<String> DEQUE = new ArrayDeque<>();

    private static final Class<?>[] KEYS = {
        boolean.class, byte.class, char.class, double.class,
        float.class, int.class, long.class, short.class, void.class,
        String.class, Object.class,
    };

    /** A LinkedHashMap subclass, the shape Spring's AnnotationAttributes uses. */
    static final class Attributes extends LinkedHashMap<String, Object> {
        Attributes() { super(); }
    }

    private static final Attributes ATTRS = new Attributes();

    private static void mapCases(String label, Map<String, String> m) {
        CASES.add(new Case(label + ".get", () -> String.valueOf(m.get("k7"))));
        CASES.add(new Case(label + ".getOrDefault", () -> String.valueOf(m.getOrDefault("k7", "?"))));
        CASES.add(new Case(label + ".containsKey", () -> String.valueOf(m.containsKey("k7"))));
        CASES.add(new Case(label + ".containsValue", () -> String.valueOf(m.containsValue("v7"))));
        CASES.add(new Case(label + ".size", () -> String.valueOf(m.size())));
        CASES.add(new Case(label + ".isEmpty", () -> String.valueOf(m.isEmpty())));
        CASES.add(new Case(label + ".keySet", () -> new java.util.TreeSet<>(m.keySet()).toString()));
        CASES.add(new Case(label + ".values.size", () -> String.valueOf(m.values().size())));
        CASES.add(new Case(label + ".entrySet.size", () -> String.valueOf(m.entrySet().size())));
        CASES.add(new Case(label + ".entry round-trip", () -> {
            StringBuilder sb = new StringBuilder();
            java.util.TreeMap<String, String> sorted = new java.util.TreeMap<>();
            for (Map.Entry<String, String> e : m.entrySet()) {
                sorted.put(e.getKey(), e.getValue());
            }
            sb.append(sorted);
            return sb.toString();
        }));
        CASES.add(new Case(label + ".forEach count", () -> {
            int[] n = {0};
            m.forEach((k, v) -> n[0]++);
            return String.valueOf(n[0]);
        }));
        CASES.add(new Case(label + ".toString len", () -> String.valueOf(m.toString().length())));
    }

    private static void listCases(String label, List<String> l) {
        CASES.add(new Case(label + ".get", () -> l.get(5)));
        CASES.add(new Case(label + ".size", () -> String.valueOf(l.size())));
        CASES.add(new Case(label + ".contains", () -> String.valueOf(l.contains("e5"))));
        CASES.add(new Case(label + ".indexOf", () -> String.valueOf(l.indexOf("e5"))));
        CASES.add(new Case(label + ".iterator count", () -> {
            int n = 0;
            for (String ignored : l) {
                n++;
            }
            return String.valueOf(n);
        }));
        CASES.add(new Case(label + ".toArray len", () -> String.valueOf(l.toArray().length)));
        CASES.add(new Case(label + ".toString len", () -> String.valueOf(l.toString().length())));
    }

    private static void setCases(String label, Set<String> s) {
        CASES.add(new Case(label + ".contains", () -> String.valueOf(s.contains("s5"))));
        CASES.add(new Case(label + ".size", () -> String.valueOf(s.size())));
        CASES.add(new Case(label + ".iterator count", () -> {
            int n = 0;
            for (String ignored : s) {
                n++;
            }
            return String.valueOf(n);
        }));
        CASES.add(new Case(label + ".toArray len", () -> String.valueOf(s.toArray().length)));
    }

    static {
        for (Class<?> k : KEYS) {
            IDENTITY.put(k, k.getName());
        }
        for (int i = 0; i < 12; i++) {
            HASH.put("k" + i, "v" + i);
            LINKED.put("k" + i, "v" + i);
            TREE.put("k" + i, "v" + i);
            CHM.put("k" + i, "v" + i);
            ATTRS.put("k" + i, "v" + i);
            ALIST.add("e" + i);
            LLIST.add("e" + i);
            HSET.add("s" + i);
            LSET.add("s" + i);
            TSET.add("s" + i);
            DEQUE.add("d" + i);
        }

        CASES.add(new Case("IdentityHashMap.get(boolean)", () -> String.valueOf(IDENTITY.get(boolean.class))));
        CASES.add(new Case("IdentityHashMap.get(int)", () -> String.valueOf(IDENTITY.get(int.class))));
        CASES.add(new Case("IdentityHashMap.get(String)", () -> String.valueOf(IDENTITY.get(String.class))));
        CASES.add(new Case("IdentityHashMap.get(absent)", () -> String.valueOf(IDENTITY.get(Integer.class))));
        CASES.add(new Case("IdentityHashMap.containsKey", () -> String.valueOf(IDENTITY.containsKey(long.class))));
        CASES.add(new Case("IdentityHashMap.size", () -> String.valueOf(IDENTITY.size())));
        CASES.add(new Case("IdentityHashMap.keySet.size", () -> String.valueOf(IDENTITY.keySet().size())));
        CASES.add(new Case("IdentityHashMap.entrySet.size", () -> String.valueOf(IDENTITY.entrySet().size())));

        mapCases("HashMap", HASH);
        mapCases("LinkedHashMap", LINKED);
        mapCases("TreeMap", TREE);
        mapCases("ConcurrentHashMap", CHM);

        // Spring's AnnotationAttributes shape: a user subclass of LinkedHashMap.
        CASES.add(new Case("Attributes.get", () -> String.valueOf(ATTRS.get("k7"))));
        CASES.add(new Case("Attributes.size", () -> String.valueOf(ATTRS.size())));
        CASES.add(new Case("Attributes.containsKey", () -> String.valueOf(ATTRS.containsKey("k7"))));
        CASES.add(new Case("Attributes.keySet", () -> new java.util.TreeSet<>(ATTRS.keySet()).toString()));
        CASES.add(new Case("Attributes.entrySet.size", () -> String.valueOf(ATTRS.entrySet().size())));

        listCases("ArrayList", ALIST);
        listCases("LinkedList", LLIST);
        setCases("HashSet", HSET);
        setCases("LinkedHashSet", LSET);
        setCases("TreeSet", TSET);

        CASES.add(new Case("ArrayDeque.peek", () -> String.valueOf(DEQUE.peek())));
        CASES.add(new Case("ArrayDeque.size", () -> String.valueOf(DEQUE.size())));
    }

    private static String[] runAll() {
        String[] out = new String[CASES.size()];
        for (int i = 0; i < CASES.size(); i++) {
            try {
                out[i] = CASES.get(i).op().get();
            } catch (Throwable t) {
                out[i] = "THREW " + t.getClass().getName() + ": " + t.getMessage();
            }
        }
        return out;
    }

    private static byte[] bootBytes(String binaryName) throws Exception {
        String resource = "/" + binaryName.replace('.', '/') + ".class";
        try (InputStream in = Object.class.getResourceAsStream(resource)) {
            if (in == null) {
                throw new IllegalStateException("cannot read " + resource + " from the runtime image");
            }
            return in.readAllBytes();
        }
    }

    private static boolean redefine(String binaryName) {
        try {
            Class<?> target = Class.forName(binaryName, false, null);
            Class<?> instrument = Class.forName("cratonvm.Instrument");
            Object applied = instrument.getMethod("redefineClass", Class.class, byte[].class)
                    .invoke(null, target, bootBytes(binaryName));
            return Boolean.TRUE.equals(applied);
        } catch (Throwable t) {
            System.out.println("REDEFINE skipped for " + binaryName + " (" + t + ")");
            return false;
        }
    }

    private static final String[] TARGETS = {
        "java.util.IdentityHashMap", "java.util.HashMap", "java.util.LinkedHashMap",
        "java.util.TreeMap", "java.util.concurrent.ConcurrentHashMap",
        "java.util.ArrayList", "java.util.LinkedList",
        "java.util.HashSet", "java.util.LinkedHashSet", "java.util.TreeSet",
        "java.util.ArrayDeque",
    };

    public static void main(String[] args) {
        String[] before = runAll();

        boolean applied = false;
        for (String c : TARGETS) {
            applied |= redefine(c);
        }
        System.out.println("REDEFINE applied=" + applied);

        String[] after = runAll();

        int diffs = 0;
        for (int i = 0; i < CASES.size(); i++) {
            boolean same = before[i].equals(after[i]);
            if (!same) {
                diffs++;
                System.out.printf("DIFF %-32s before[%s]  after[%s]%n",
                        CASES.get(i).name(), before[i], after[i]);
            }
        }
        System.out.println("checked " + CASES.size() + " operations");
        System.out.println(diffs == 0
                ? "PROBE PASS"
                : "PROBE FAIL " + diffs + " operation(s) changed behaviour across the redefinition");
        if (diffs != 0) {
            System.exit(1);
        }
    }
}
