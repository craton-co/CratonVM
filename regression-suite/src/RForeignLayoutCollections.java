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
 * every observable is printed as `CK RForeignLayoutCollections key=value` so a
 * run diffs byte-for-byte against real HotSpot, and each subclass counts the
 * calls that reach it, since the previous two probes both found defects the
 * return values alone hid.
 *
 * THE PREFIX IS LOAD-BEARING. run.sh's cross-VM extractor is
 * `grep -aE '^(PASS|CK) '`, so a bare `key=value` line is DELETED before the
 * diff ever sees it. This vector printed all 42 of its observables that way and
 * held zero `check()` calls, which reduced it to the constant string
 * `PASS RForeignLayoutCollections` — a green that survived every possible
 * answer, including the silent-empty one it exists to catch. Every observable
 * now carries the prefix AND is asserted locally against the value measured on
 * HotSpot 25, because run.sh skips the diff outright when no HotSpot is present
 * (it says so out loud) and a vector that can only fail through the diff is
 * half-armed on such a host.
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
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RForeignLayoutCollections: " + m);
        }
    }

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

    /**
     * Print one observable on a line run.sh's `^(PASS|CK) ` extractor keeps.
     * The value is always whatever the call under test produced — including the
     * `EXC:` token `safe` builds from a thrown Throwable, which used to be
     * filtered away along with everything else.
     */
    private static void ck(String key, String value) {
        System.out.println("CK RForeignLayoutCollections " + key + "=" + value);
    }

    /**
     * Run one observable, print it, and assert it equals `want` — the value
     * MEASURED on HotSpot 25 (Adoptium 25.0.3.9), never a guess. The print is
     * for the cross-VM diff; the assertion is what keeps the vector armed on a
     * host with no HotSpot, where run.sh skips the diff entirely.
     */
    private static void ckEq(String key, String want, java.util.function.Supplier<String> f) {
        String got = safe(f);
        ck(key, got);
        check(want.equals(got), key + " = " + got + ", want " + want);
    }

    public static void main(String[] args) {
        // ---- AbstractCollection: every method below is inherited concrete ----
        ForeignCollection c = new ForeignCollection("a,b,c");
        ckEq("coll.size", "3", () -> String.valueOf(c.size()));
        ckEq("coll.isEmpty", "false", () -> String.valueOf(c.isEmpty()));
        ckEq("coll.contains.b", "true", () -> String.valueOf(c.contains("b")));
        ckEq("coll.contains.zz", "false", () -> String.valueOf(c.contains("zz")));
        ckEq("coll.containsAll.ab", "true",
                () -> String.valueOf(c.containsAll(Arrays.asList("a", "b"))));
        ckEq("coll.containsAll.zz", "false",
                () -> String.valueOf(c.containsAll(Arrays.asList("zz"))));
        ckEq("coll.toArray.len", "3", () -> String.valueOf(c.toArray().length));
        ckEq("coll.toArray.join", "a|b|c",
                () -> String.join("|", Arrays.stream(c.toArray())
                        .map(String::valueOf).toArray(String[]::new)));
        ckEq("coll.toString", "[a, b, c]", c::toString);
        ckEq("coll.stream.count", "3", () -> String.valueOf(c.stream().count()));

        // ---- AbstractSet: adds hashCode/equals over AbstractCollection ------
        ForeignSet s = new ForeignSet("a,b,c");
        ForeignSet s2 = new ForeignSet("a,b,c");
        ckEq("set.size", "3", () -> String.valueOf(s.size()));
        ckEq("set.contains.b", "true", () -> String.valueOf(s.contains("b")));
        ckEq("set.hashCode.isZero", "false", () -> String.valueOf(s.hashCode() == 0));
        // The specified value: the sum of the elements' hashes.
        ckEq("set.hashCode.matchesSpec", "true",
                () -> {
            int want = "a".hashCode() + "b".hashCode() + "c".hashCode();
            return String.valueOf(s.hashCode() == want);
        });
        ckEq("set.equalsSameContents", "true", () -> String.valueOf(s.equals(s2)));
        ckEq("set.equalsRealSet", "true", () -> String.valueOf(s.equals(Set.of("a", "b", "c"))));
        ckEq("set.toString", "[a, b, c]", s::toString);

        // ---- AbstractList: adds indexOf/equals/hashCode/subList ------------
        ForeignList l = new ForeignList("a,b,c");
        ckEq("list.size", "3", () -> String.valueOf(l.size()));
        ckEq("list.get1", "b", () -> l.get(1));
        ckEq("list.indexOf.b", "1", () -> String.valueOf(l.indexOf("b")));
        ckEq("list.indexOf.zz", "-1", () -> String.valueOf(l.indexOf("zz")));
        ckEq("list.contains.c", "true", () -> String.valueOf(l.contains("c")));
        ckEq("list.equalsRealList", "true",
                () -> String.valueOf(l.equals(Arrays.asList("a", "b", "c"))));
        ckEq("list.hashCode.matchesSpec", "true",
                () -> {
            int want = Arrays.asList("a", "b", "c").hashCode();
            return String.valueOf(l.hashCode() == want);
        });
        ckEq("list.subList.size", "2", () -> String.valueOf(l.subList(1, 3).size()));
        ckEq("list.toArray.len", "3", () -> String.valueOf(l.toArray().length));
        ckEq("list.toString", "[a, b, c]", l::toString);

        // ---- AbstractMap ---------------------------------------------------
        ForeignMap m = new ForeignMap("k1=v1,k2=v2");
        ckEq("map.size", "2", () -> String.valueOf(m.size()));
        ckEq("map.isEmpty", "false", () -> String.valueOf(m.isEmpty()));
        ckEq("map.get.k1", "v1", () -> String.valueOf(m.get("k1")));
        ckEq("map.get.zz", "null", () -> String.valueOf(m.get("zz")));
        ckEq("map.containsKey.k2", "true", () -> String.valueOf(m.containsKey("k2")));
        ckEq("map.containsValue.v1", "true", () -> String.valueOf(m.containsValue("v1")));
        ckEq("map.keySet.size", "2", () -> String.valueOf(m.keySet().size()));
        ckEq("map.values.size", "2", () -> String.valueOf(m.values().size()));
        ckEq("map.equalsRealMap", "true",
                () -> String.valueOf(m.equals(Map.of("k1", "v1", "k2", "v2"))));
        ckEq("map.toString", "{k1=v1, k2=v2}", m::toString);

        // ---- did the calls reach the application's own bodies? -------------
        // Booleans, not raw call counts. "Did the native ask the object" is
        // the property; HOW MANY times a concrete AbstractCollection method
        // happens to call iterator() is an implementation detail, and pinning
        // it byte-for-byte in the cross-VM diff would fail a CratonVM that is
        // correct but reaches the same answer differently.
        // Zeroes here with plausible answers above is the silent-empty failure:
        // a native answered from a layout it invented for a class it has never
        // seen, instead of asking the object.
        ckEq("count.coll.iterator", "true", () -> String.valueOf(c.iterators > 0));
        ckEq("count.coll.size", "true", () -> String.valueOf((c.sizes > 0)));
        ckEq("count.set.iterator", "true", () -> String.valueOf(s.iterators > 0));
        ckEq("count.list.get", "true", () -> String.valueOf((l.gets > 0)));
        ckEq("count.map.entrySet", "true", () -> String.valueOf(m.entrySets > 0));
        System.out.println("CK RForeignLayoutCollections checks=" + checks);
        System.out.println("PASS RForeignLayoutCollections (" + checks + " checks)");
    }
}
