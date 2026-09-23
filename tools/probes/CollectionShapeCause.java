import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Hashtable;
import java.util.HashMap;
import java.util.HashSet;
import java.util.IdentityHashMap;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.Locale;
import java.util.PriorityQueue;
import java.util.Properties;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.Vector;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentLinkedDeque;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.ConcurrentSkipListMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CopyOnWriteArraySet;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.function.Consumer;
import java.util.function.Supplier;

/**
 * WHY a collection instance is wide, not just THAT it is.
 *
 * `AllocShapeTruth` reads retained heap per instance, which is the right
 * number and cannot distinguish its two causes: an object with too many SLOTS,
 * and an object of the right width that eagerly RETAINS an array HotSpot
 * leaves null. `docs/internal/fixed-bugs/jdk-collection-classes-are-padded-to-
 * a-synthetic-stub-floor-FIXED-20260911.md` was opened attributing both of its
 * TreeMap (352 B) and IdentityHashMap (584 B) rows to a field-count ratchet;
 * this probe is what showed neither was one -- the first is an eagerly
 * allocated backing array, and the second is an array HOTSPOT allocates too,
 * at 8-byte references instead of 4.
 *
 * Two tables, because the two causes separate differently in each. EMPTY is
 * where an eager constructor allocation shows up against a HotSpot that
 * allocates nothing. FILLED (four entries) is where the per-entry carrier
 * shows up -- a node, a bucket array, a segment -- and where an empty-shape
 * win can turn out to have been moved rather than removed.
 *
 * Run it against HotSpot and CratonVM and diff. `--add-opens
 * java.base/java.util=ALL-UNNAMED` is needed for the field values; without it
 * the retained tables still print and the field dump says
 * `InaccessibleObjectException`.
 */
public class CollectionShapeCause {

    static Object[] keep;

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 50000;

        System.out.println(String.format("%-32s %10s", "shape (empty)", "retained"));
        row("ArrayList", n, ArrayList::new);
        row("Vector", n, Vector::new);
        row("LinkedList", n, LinkedList::new);
        row("ArrayDeque", n, ArrayDeque::new);
        row("PriorityQueue", n, PriorityQueue::new);
        row("HashMap", n, HashMap::new);
        row("LinkedHashMap", n, LinkedHashMap::new);
        row("Hashtable", n, Hashtable::new);
        row("Properties", n, Properties::new);
        row("TreeMap", n, TreeMap::new);
        row("IdentityHashMap", n, IdentityHashMap::new);
        row("HashSet", n, HashSet::new);
        row("LinkedHashSet", n, LinkedHashSet::new);
        row("TreeSet", n, TreeSet::new);
        row("ConcurrentHashMap", n, ConcurrentHashMap::new);
        row("ConcurrentSkipListMap", n, ConcurrentSkipListMap::new);
        row("CopyOnWriteArrayList", n, CopyOnWriteArrayList::new);
        row("CopyOnWriteArraySet", n, CopyOnWriteArraySet::new);
        row("ConcurrentLinkedQueue", n, ConcurrentLinkedQueue::new);
        row("ConcurrentLinkedDeque", n, ConcurrentLinkedDeque::new);
        row("LinkedBlockingQueue", n, LinkedBlockingQueue::new);
        System.out.println();

        System.out.println(String.format("%-32s %10s", "shape (4 entries)", "retained"));
        filled("ArrayList+4", n, ArrayList::new, CollectionShapeCause::addFour);
        filled("Vector+4", n, Vector::new, CollectionShapeCause::addFour);
        filled("LinkedList+4", n, LinkedList::new, CollectionShapeCause::addFour);
        filled("ArrayDeque+4", n, ArrayDeque::new, CollectionShapeCause::addFour);
        filled("HashMap+4", n, HashMap::new, CollectionShapeCause::putFour);
        filled("LinkedHashMap+4", n, LinkedHashMap::new, CollectionShapeCause::putFour);
        filled("Hashtable+4", n, Hashtable::new, CollectionShapeCause::putFour);
        // The EMPTY row alone cannot tell a deferred allocation from a deleted
        // one, and `Properties` is the class where the difference is the whole
        // question: its entries do NOT live in the bucket table its empty row
        // used to retain, so if the empty row falls and this one does not rise
        // to meet it, the array was never carrying anything.
        filled("Properties+4", n, Properties::new, CollectionShapeCause::putFour);
        filled("TreeMap+4", n, TreeMap::new, CollectionShapeCause::putFour);
        filled("HashSet+4", n, HashSet::new, CollectionShapeCause::addFour);
        filled("LinkedHashSet+4", n, LinkedHashSet::new, CollectionShapeCause::addFour);
        filled("TreeSet+4", n, TreeSet::new, CollectionShapeCause::addFour);
        filled("ConcurrentHashMap+4", n, ConcurrentHashMap::new, CollectionShapeCause::putFour);
        filled("CopyOnWriteArrayList+4", n, CopyOnWriteArrayList::new,
                CollectionShapeCause::addFour);
        // Its EMPTY row is two tables up; this is the half that says whether a
        // backing-object change moved the cost or removed it. A
        // `CopyOnWriteArraySet` retains one `CopyOnWriteArrayList` on HotSpot,
        // and retained a `LinkedHashMap` on this VM until 2026-09-12.
        filled("CopyOnWriteArraySet+4", n, CopyOnWriteArraySet::new,
                CollectionShapeCause::addFour);
        filled("ConcurrentLinkedQueue+4", n, ConcurrentLinkedQueue::new,
                CollectionShapeCause::addFour);
        System.out.println();

        fields("java.util.ArrayList", new ArrayList<>());
        fields("java.util.Vector", new Vector<>());
        fields("java.util.ArrayDeque", new ArrayDeque<>());
        fields("java.util.HashMap", new HashMap<>());
        fields("java.util.LinkedHashMap", new LinkedHashMap<>());
        fields("java.util.Hashtable", new Hashtable<>());
        // `Properties` is the one class in this list whose real backing is NOT
        // the fields it inherits: JDK 9+ keeps its entries in a side
        // `ConcurrentHashMap map` and leaves every `Hashtable` field null, so
        // a non-null `table` here is this VM's own allocation and nothing
        // reads it through those fields.
        fields("java.util.Properties", new Properties());
        fields("java.util.HashSet", new HashSet<>());
        fields("java.util.LinkedHashSet", new LinkedHashSet<>());
        fields("java.util.TreeMap", new TreeMap<>());
        fields("java.util.TreeSet", new TreeSet<>());
        fields("java.util.IdentityHashMap", new IdentityHashMap<>());
        fields("java.util.concurrent.ConcurrentHashMap", new ConcurrentHashMap<>());
        fields("java.util.concurrent.CopyOnWriteArrayList", new CopyOnWriteArrayList<>());
        fields("java.util.concurrent.ConcurrentLinkedQueue", new ConcurrentLinkedQueue<>());
        System.out.println("CAUSE_END");
    }

    /// SHARED keys and values, allocated once.
    ///
    /// `"e" + j` inside the loop allocates four fresh Strings per instance,
    /// roughly 160 B, which is larger than most of the carriers this table is
    /// trying to weigh and would swamp every row with the same constant.
    static final String[] KEYS = {"k0", "k1", "k2", "k3"};
    static final String[] VALS = {"v0", "v1", "v2", "v3"};

    @SuppressWarnings("unchecked")
    static void addFour(Object o) {
        java.util.Collection<Object> c = (java.util.Collection<Object>) o;
        for (int j = 0; j < 4; j++) {
            c.add(KEYS[j]);
        }
    }

    @SuppressWarnings("unchecked")
    static void putFour(Object o) {
        java.util.Map<Object, Object> m = (java.util.Map<Object, Object>) o;
        for (int j = 0; j < 4; j++) {
            m.put(KEYS[j], VALS[j]);
        }
    }

    static void row(String name, int n, Supplier<Object> make) throws Exception {
        measure(name, n, make, null);
    }

    static void filled(String name, int n, Supplier<Object> make, Consumer<Object> fill)
            throws Exception {
        measure(name, n, make, fill);
    }

    static void measure(String name, int n, Supplier<Object> make, Consumer<Object> fill)
            throws Exception {
        for (int i = 0; i < 2000; i++) {
            Object o = make.get();
            if (fill != null) {
                fill.accept(o);
            }
        }
        Runtime rt = Runtime.getRuntime();
        Object[] hold = new Object[n];
        keep = hold;
        settle(rt);
        long h0 = rt.totalMemory() - rt.freeMemory();
        for (int i = 0; i < n; i++) {
            Object o = make.get();
            if (fill != null) {
                fill.accept(o);
            }
            hold[i] = o;
        }
        settle(rt);
        long retained = (rt.totalMemory() - rt.freeMemory()) - h0;
        if (keep.length != n) {
            throw new IllegalStateException("unreachable");
        }
        System.out.println(String.format(Locale.ROOT, "%-32s %10.1f", name, retained / (double) n));
        keep = null;
    }

    static void fields(String label, Object o) {
        System.out.println("== " + label);
        int slots = 0;
        for (Class<?> c = o.getClass(); c != null && c != Object.class; c = c.getSuperclass()) {
            for (Field f : c.getDeclaredFields()) {
                if (Modifier.isStatic(f.getModifiers())) {
                    continue;
                }
                slots++;
                String v;
                try {
                    f.setAccessible(true);
                    v = describe(f.get(o));
                } catch (Throwable t) {
                    v = "<" + t.getClass().getSimpleName() + ">";
                }
                System.out.println(String.format("   %-34s %-20s %s",
                        c.getSimpleName() + "." + f.getName(), f.getType().getSimpleName(), v));
            }
        }
        System.out.println("   declared instance fields: " + slots);
    }

    static String describe(Object x) {
        if (x == null) {
            return "null";
        }
        if (x.getClass().isArray()) {
            return x.getClass().getSimpleName() + " len=" + Array.getLength(x);
        }
        return x.getClass().getSimpleName() + " " + x;
    }

    static void settle(Runtime rt) throws Exception {
        for (int i = 0; i < 3; i++) {
            System.gc();
            Thread.sleep(40);
        }
    }
}
