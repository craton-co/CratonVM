// Scheduled guard for a CLOSED hazard: the natives registered on
// `java.util.Abstract*` intercept every USER subclass, not just `java.util`'s
// own, and this vector's subclasses hold their elements in a layout nothing in
// the VM models — a comma-separated `String`, split on demand.
//
// The hazard was real enough to be filed twice and worried about in a standing
// note: `collect_collection_elements` returns an empty vec for an unmodelled
// layout, which makes `containsAll` vacuously true and `AbstractSet.hashCode`
// answer 0 — a wrong-bucket miss that surfaces far away as an NPE on a null
// `Map.get`. Measured, all 42 lines match HotSpot, and the reason is NOT that
// the natives failed to run: the census shows ten of them running on a layout
// none of them models, answering correctly through
// `collect_collection_elements_or_real`, which falls back to the collection's
// own `size()`/`toArray()`.
//
// So the value here is the CONTRAST, and it is the reason this is a scheduled
// vector rather than a loose probe. The identical shape one package over IS
// broken: `java.lang.Process`'s concrete natives answer for a user subclass out
// of the VM's fixed field layout. The difference is not dispatch — in both
// cases the native is registered on the supertype and dispatch reaches it. It
// is whether the native ASKS THE RECEIVER or INDEXES INTO A LAYOUT IT ASSUMES.
// The first is safe for arbitrary subclasses; the second is a defect waiting
// for its first foreign receiver. Every line below that still matches HotSpot
// is one more native that asks.
//
// Anything that regresses here is a native that started assuming.
import java.util.AbstractCollection;
import java.util.AbstractList;
import java.util.AbstractMap;
import java.util.AbstractSet;
import java.util.Arrays;
import java.util.Iterator;
import java.util.Map;
import java.util.Set;

/**
 * Do the natives registered on `java.util.Abstract*` answer correctly for a
 * subclass whose field layout CratonVM has never seen?
 *
 * CratonVM registers 97 natives across `AbstractCollection`, `AbstractList`,
 * `AbstractSet`, `AbstractMap`, `AbstractQueue` and `AbstractSequentialList`,
 * because that is where a concrete collection's inherited method resolves. The
 * consequence is that they also stand in front of every **application**
 * subclass, and those have arbitrary layouts. `collect_collection_elements`
 * models a fixed set of known shapes and returns an EMPTY vec for anything
 * else, which makes `containsAll` vacuously true and `hashCode` zero — neither
 * of which throws.
 *
 * `collect_collection_elements_or_real` exists for exactly this and falls back
 * to the collection's real `size()`/`toArray()`. Its doc records the structural
 * limit: it is only usable for a collection passed as an **argument**, never for
 * the **receiver** of an element-reading native, because driving the receiver's
 * own `toArray()` would re-enter the native being served. So receiver-reading
 * natives on these classes have no such escape, and this probe is aimed at them.
 *
 * Shape follows `UserImplementorInterceptProbe` / `UserProcessInterceptProbe`:
 * every observable is printed as `key=value` so a run diffs byte-for-byte
 * against real HotSpot, and each subclass counts the calls that reach it, since
 * the previous two probes both found defects the return values alone hid.
 *
 * The subclasses deliberately implement ONLY what the abstract class leaves
 * abstract, and hold their data in a field CratonVM cannot recognise — a
 * `String` holding comma-separated values, decoded on demand. Nothing here
 * matches any known collection layout.
 *
 *   javac -d out probes/RForeignLayoutCollections.java
 *   java  -cp out RForeignLayoutCollections            # control
 *   cratonvm --real-jdk --java-home $JDK -cp out RForeignLayoutCollections
 */
public class RForeignLayoutCollections {

    /** Elements live in a `String`, not in any array CratonVM models. */
    static final class ForeignCollection extends AbstractCollection<String> {
        final String packed;
        int iterators, sizes;

        ForeignCollection(String packed) { this.packed = packed; }

        @Override public Iterator<String> iterator() {
            iterators++;
            return Arrays.asList(packed.split(",")).iterator();
        }

        @Override public int size() { sizes++; return packed.split(",").length; }
    }

    static final class ForeignSet extends AbstractSet<String> {
        final String packed;
        int iterators, sizes;

        ForeignSet(String packed) { this.packed = packed; }

        @Override public Iterator<String> iterator() {
            iterators++;
            return Arrays.asList(packed.split(",")).iterator();
        }

        @Override public int size() { sizes++; return packed.split(",").length; }
    }

    static final class ForeignList extends AbstractList<String> {
        final String packed;
        int gets, sizes;

        ForeignList(String packed) { this.packed = packed; }

        @Override public String get(int index) { gets++; return packed.split(",")[index]; }
        @Override public int size() { sizes++; return packed.split(",").length; }
    }

    static final class ForeignMap extends AbstractMap<String, String> {
        final String packed;
        int entrySets;

        ForeignMap(String packed) { this.packed = packed; }

        @Override public Set<Entry<String, String>> entrySet() {
            entrySets++;
            java.util.LinkedHashSet<Entry<String, String>> out = new java.util.LinkedHashSet<>();
            for (String kv : packed.split(",")) {
                String[] p = kv.split("=");
                out.add(new AbstractMap.SimpleEntry<>(p[0], p[1]));
            }
            return out;
        }
    }

    private static String safe(java.util.function.Supplier<String> f) {
        try {
            return f.get();
        } catch (Throwable t) {
            return "EXC:" + t.getClass().getName();
        }
    }

    public static void main(String[] args) {
        // ---- AbstractCollection: every method below is inherited concrete ----
        ForeignCollection c = new ForeignCollection("a,b,c");
        System.out.println("coll.size=" + safe(() -> String.valueOf(c.size())));
        System.out.println("coll.isEmpty=" + safe(() -> String.valueOf(c.isEmpty())));
        System.out.println("coll.contains.b=" + safe(() -> String.valueOf(c.contains("b"))));
        System.out.println("coll.contains.zz=" + safe(() -> String.valueOf(c.contains("zz"))));
        System.out.println("coll.containsAll.ab="
                + safe(() -> String.valueOf(c.containsAll(Arrays.asList("a", "b")))));
        System.out.println("coll.containsAll.zz="
                + safe(() -> String.valueOf(c.containsAll(Arrays.asList("zz")))));
        System.out.println("coll.toArray.len="
                + safe(() -> String.valueOf(c.toArray().length)));
        System.out.println("coll.toArray.join="
                + safe(() -> String.join("|", Arrays.stream(c.toArray())
                        .map(String::valueOf).toArray(String[]::new))));
        System.out.println("coll.toString=" + safe(c::toString));
        System.out.println("coll.stream.count="
                + safe(() -> String.valueOf(c.stream().count())));

        // ---- AbstractSet: adds hashCode/equals over AbstractCollection ------
        ForeignSet s = new ForeignSet("a,b,c");
        ForeignSet s2 = new ForeignSet("a,b,c");
        System.out.println("set.size=" + safe(() -> String.valueOf(s.size())));
        System.out.println("set.contains.b=" + safe(() -> String.valueOf(s.contains("b"))));
        System.out.println("set.hashCode.isZero="
                + safe(() -> String.valueOf(s.hashCode() == 0)));
        // The specified value: the sum of the elements' hashes.
        System.out.println("set.hashCode.matchesSpec=" + safe(() -> {
            int want = "a".hashCode() + "b".hashCode() + "c".hashCode();
            return String.valueOf(s.hashCode() == want);
        }));
        System.out.println("set.equalsSameContents="
                + safe(() -> String.valueOf(s.equals(s2))));
        System.out.println("set.equalsRealSet="
                + safe(() -> String.valueOf(s.equals(Set.of("a", "b", "c")))));
        System.out.println("set.toString=" + safe(s::toString));

        // ---- AbstractList: adds indexOf/equals/hashCode/subList ------------
        ForeignList l = new ForeignList("a,b,c");
        System.out.println("list.size=" + safe(() -> String.valueOf(l.size())));
        System.out.println("list.get1=" + safe(() -> l.get(1)));
        System.out.println("list.indexOf.b=" + safe(() -> String.valueOf(l.indexOf("b"))));
        System.out.println("list.indexOf.zz=" + safe(() -> String.valueOf(l.indexOf("zz"))));
        System.out.println("list.contains.c=" + safe(() -> String.valueOf(l.contains("c"))));
        System.out.println("list.equalsRealList="
                + safe(() -> String.valueOf(l.equals(Arrays.asList("a", "b", "c")))));
        System.out.println("list.hashCode.matchesSpec=" + safe(() -> {
            int want = Arrays.asList("a", "b", "c").hashCode();
            return String.valueOf(l.hashCode() == want);
        }));
        System.out.println("list.subList.size="
                + safe(() -> String.valueOf(l.subList(1, 3).size())));
        System.out.println("list.toArray.len=" + safe(() -> String.valueOf(l.toArray().length)));
        System.out.println("list.toString=" + safe(l::toString));

        // ---- AbstractMap ---------------------------------------------------
        ForeignMap m = new ForeignMap("k1=v1,k2=v2");
        System.out.println("map.size=" + safe(() -> String.valueOf(m.size())));
        System.out.println("map.isEmpty=" + safe(() -> String.valueOf(m.isEmpty())));
        System.out.println("map.get.k1=" + safe(() -> String.valueOf(m.get("k1"))));
        System.out.println("map.get.zz=" + safe(() -> String.valueOf(m.get("zz"))));
        System.out.println("map.containsKey.k2="
                + safe(() -> String.valueOf(m.containsKey("k2"))));
        System.out.println("map.containsValue.v1="
                + safe(() -> String.valueOf(m.containsValue("v1"))));
        System.out.println("map.keySet.size="
                + safe(() -> String.valueOf(m.keySet().size())));
        System.out.println("map.values.size="
                + safe(() -> String.valueOf(m.values().size())));
        System.out.println("map.equalsRealMap="
                + safe(() -> String.valueOf(m.equals(Map.of("k1", "v1", "k2", "v2")))));
        System.out.println("map.toString=" + safe(m::toString));

        // ---- did the calls reach the application's own bodies? -------------
        // Zeroes here with plausible answers above is the silent-empty failure:
        // a native answered from a layout it invented for a class it has never
        // seen, instead of asking the object.
        System.out.println("count.coll.iterator=" + c.iterators);
        System.out.println("count.coll.size=" + (c.sizes > 0));
        System.out.println("count.set.iterator=" + s.iterators);
        System.out.println("count.list.get=" + (l.gets > 0));
        System.out.println("count.map.entrySet=" + m.entrySets);
        System.out.println("PASS RForeignLayoutCollections");
    }
}
