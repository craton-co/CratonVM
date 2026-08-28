import java.util.*;
import java.util.function.Consumer;

/** The `java.util.Arrays` and `java.util.HashSet` triples that the
 *  `--jdk-only-report` says a BRIDGE NATIVE ACTUALLY WON, i.e. ran in front of
 *  real JDK bytecode rather than losing the dispatch to it.
 *
 *  This is Phase 2's worklist, not a family picked by hand. The report's shadow
 *  rows carry an `outcome` field; filtering to `native-won` over five probe runs
 *  gave 334 distinct triples, and these 21 are the two most tractable families
 *  at the top of it:
 *
 *    Arrays   asList, copyOf x3, copyOfRange([BII), equals([B[B), fill x2, hashCode([B)
 *    HashSet  <init> x2, add, addAll, contains, forEach, hashCode, isEmpty,
 *             iterator, remove, size, toArray
 *
 *  WHAT THIS PROBE DOES AND DOES NOT DECIDE. A clean diff is a PRECONDITION for
 *  retiring a shadow, never a justification on its own: a native may exist
 *  because the bytecode path was measured slower, or because it was measured
 *  WRONG once and the native is the fix. So this establishes only that the two
 *  agree on behaviour; the retirement decision needs the registrar's history and
 *  an invocation count beside it.
 *
 *  Every question is asked at the boundaries, because the middle of these
 *  contracts is where a shim is most likely to be right and the edges are where
 *  it is most likely to have been written from memory: nulls, empties, negative
 *  and over-long lengths, aliasing, identity-vs-equals, and the exact exception
 *  type each refusal must produce.
 */
public class ArraysHashSetShadowSweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }
    static String hex(byte[] b) {
        if (b == null) return "null";
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x));
        return s.toString();
    }

    // ---- Arrays.copyOf([BI) and copyOfRange([BII) -----------------------
    static void byteCopies() {
        byte[] src = {1, 2, 3, 4, 5};
        p("copyOf B same len", hex(Arrays.copyOf(src, 5)));
        p("copyOf B shorter", hex(Arrays.copyOf(src, 3)));
        p("copyOf B longer (zero pad)", hex(Arrays.copyOf(src, 8)));
        p("copyOf B zero", hex(Arrays.copyOf(src, 0)));
        p("copyOf B is a COPY", Arrays.copyOf(src, 5) == src);
        p("copyOf B empty source", hex(Arrays.copyOf(new byte[0], 3)));
        t("copyOf B negative", () -> Arrays.copyOf(src, -1));
        t("copyOf B null", () -> Arrays.copyOf((byte[]) null, 1));

        p("copyOfRange B mid", hex(Arrays.copyOfRange(src, 1, 4)));
        p("copyOfRange B full", hex(Arrays.copyOfRange(src, 0, 5)));
        p("copyOfRange B empty at end", hex(Arrays.copyOfRange(src, 5, 5)));
        // to > length is LEGAL and zero-pads; that is the row a from-memory
        // implementation most often gets wrong by bounds-checking `to`.
        p("copyOfRange B past end pads", hex(Arrays.copyOfRange(src, 3, 9)));
        p("copyOfRange B from==to==0", hex(Arrays.copyOfRange(src, 0, 0)));
        t("copyOfRange B from>to", () -> Arrays.copyOfRange(src, 3, 1));
        t("copyOfRange B negative from", () -> Arrays.copyOfRange(src, -1, 2));
        t("copyOfRange B from>length", () -> Arrays.copyOfRange(src, 9, 9));
        t("copyOfRange B null", () -> Arrays.copyOfRange((byte[]) null, 0, 1));
    }

    // ---- Arrays.copyOf(Object[], int) and the 3-arg Class form ----------
    static void objectCopies() {
        Object[] src = {"a", "b", "c"};
        p("copyOf O shorter", Arrays.toString(Arrays.copyOf(src, 2)));
        p("copyOf O longer (null pad)", Arrays.toString(Arrays.copyOf(src, 5)));
        p("copyOf O runtime type", Arrays.copyOf(src, 2).getClass().getName());
        String[] strs = {"x", "y"};
        // The 2-arg form PRESERVES the runtime component type -- copying a
        // String[] must give a String[], not an Object[].
        p("copyOf String[] keeps type", Arrays.copyOf(strs, 3).getClass().getName());
        p("copyOf String[] store check", storeIntoCopy(strs));
        t("copyOf O negative", () -> Arrays.copyOf(src, -1));
        t("copyOf O null", () -> Arrays.copyOf((Object[]) null, 1));

        // The 3-arg form takes the target ARRAY class, not the component class.
        p("copyOf 3-arg to Object[]",
          Arrays.toString(Arrays.copyOf(strs, 2, Object[].class)));
        p("copyOf 3-arg to Object[] type",
          Arrays.copyOf(strs, 2, Object[].class).getClass().getName());
        p("copyOf 3-arg to String[] type",
          Arrays.copyOf(strs, 2, String[].class).getClass().getName());
        p("copyOf 3-arg widen pads null",
          Arrays.toString(Arrays.copyOf(strs, 4, Object[].class)));
        Object[] mixed = {"s", Integer.valueOf(1)};
        t("copyOf 3-arg incompatible", () -> Arrays.copyOf(mixed, 2, String[].class));
        t("copyOf 3-arg null class", () -> Arrays.copyOf(strs, 2, null));
    }
    /** A `String[]` copy must REFUSE an Integer, proving it is not an Object[]. */
    static String storeIntoCopy(String[] s) {
        Object[] c = Arrays.copyOf(s, 2);
        try { c[0] = Integer.valueOf(1); return "accepted an Integer"; }
        catch (ArrayStoreException e) { return "ArrayStoreException"; }
    }

    // ---- Arrays.equals([B[B), hashCode([B), fill, asList ----------------
    static void equalsHashFill() {
        p("equals B same", Arrays.equals(new byte[]{1, 2}, new byte[]{1, 2}));
        p("equals B differ", Arrays.equals(new byte[]{1, 2}, new byte[]{1, 3}));
        p("equals B length differ", Arrays.equals(new byte[]{1}, new byte[]{1, 2}));
        p("equals B both empty", Arrays.equals(new byte[0], new byte[0]));
        p("equals B both null", Arrays.equals((byte[]) null, (byte[]) null));
        p("equals B one null", Arrays.equals(new byte[]{1}, (byte[]) null));
        p("equals B same identity", Arrays.equals(new byte[]{7}, new byte[]{7}));

        p("hashCode B empty", Arrays.hashCode(new byte[0]));
        p("hashCode B null", Arrays.hashCode((byte[]) null));
        p("hashCode B {1,2,3}", Arrays.hashCode(new byte[]{1, 2, 3}));
        p("hashCode B {0}", Arrays.hashCode(new byte[]{0}));
        p("hashCode B negative byte", Arrays.hashCode(new byte[]{-1, -128}));
        p("hashCode B order matters",
          Arrays.hashCode(new byte[]{1, 2}) == Arrays.hashCode(new byte[]{2, 1}));

        int[] ints = new int[4];
        Arrays.fill(ints, 7);
        p("fill int", Arrays.toString(ints));
        int[] empty = new int[0];
        Arrays.fill(empty, 1);
        p("fill int empty", Arrays.toString(empty));
        t("fill int null", () -> Arrays.fill((int[]) null, 1));
        Object[] objs = new Object[3];
        Arrays.fill(objs, "z");
        p("fill Object", Arrays.toString(objs));
        Arrays.fill(objs, null);
        p("fill Object null value", Arrays.toString(objs));
        String[] typed = new String[2];
        t("fill typed with wrong type", () -> Arrays.fill((Object[]) typed, Integer.valueOf(1)));

        List<String> al = Arrays.asList("a", "b", "c");
        p("asList contents", al.toString());
        p("asList size", al.size());
        p("asList class", al.getClass().getName());
        p("asList get", al.get(1));
        p("asList indexOf", al.indexOf("c"));
        p("asList contains", al.contains("b"));
        // FIXED-SIZE but MUTABLE: set is allowed, add/remove are not.
        al.set(0, "z");
        p("asList set allowed", al.toString());
        t("asList add refused", () -> al.add("d"));
        t("asList remove refused", () -> al.remove("a"));
        t("asList clear refused", () -> al.clear());
        // asList WRAPS the array: a write through the list is visible in it.
        String[] backing = {"p", "q"};
        List<String> view = Arrays.asList(backing);
        view.set(1, "Q");
        p("asList writes through to array", Arrays.toString(backing));
        p("asList of empty", Arrays.asList().toString());
        p("asList single null", Arrays.asList((Object) null).toString());
        p("asList equals ArrayList", Arrays.asList("a", "b").equals(new ArrayList<>(List.of("a", "b"))));
        p("asList hashCode matches List", Arrays.asList("a", "b").hashCode()
            == new ArrayList<>(List.of("a", "b")).hashCode());
    }

    // ---- HashSet, the whole shadowed surface ---------------------------
    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) l.add(String.valueOf(o));
        Collections.sort(l);
        return l.toString();
    }
    static void hashSet() {
        HashSet<String> s = new HashSet<>();
        p("new HashSet isEmpty", s.isEmpty());
        p("new HashSet size", s.size());
        p("add first", s.add("a"));
        p("add duplicate", s.add("a"));
        p("size after dup", s.size());
        p("contains present", s.contains("a"));
        p("contains absent", s.contains("zz"));
        p("contains null (absent)", s.contains(null));
        p("add null", s.add(null));
        p("add null twice", s.add(null));
        p("contains null (present)", s.contains(null));
        p("size with null", s.size());
        p("remove null", s.remove(null));
        p("remove absent", s.remove("zz"));
        p("remove present", s.remove("a"));
        p("isEmpty after removals", s.isEmpty());

        HashSet<String> t2 = new HashSet<>(Arrays.asList("a", "b", "c", "a"));
        p("ctor from Collection dedups", t2.size());
        p("ctor from Collection contents", sorted(t2));
        p("addAll new elements", t2.addAll(Arrays.asList("d", "e")));
        p("addAll all duplicates", t2.addAll(Arrays.asList("a", "b")));
        p("addAll empty", t2.addAll(new ArrayList<>()));
        p("contents after addAll", sorted(t2));
        p("toArray length", t2.toArray().length);
        p("toArray sorted", sorted(Arrays.asList(t2.toArray())));
        p("toArray runtime type", t2.toArray().getClass().getName());

        StringBuilder seen = new StringBuilder();
        List<String> collected = new ArrayList<>();
        Consumer<String> c = collected::add;
        t2.forEach(c);
        Collections.sort(collected);
        p("forEach visited all", collected.toString());
        p("forEach count == size", collected.size() == t2.size());
        t("forEach null action", () -> t2.forEach(null));

        // hashCode is the SUM of element hashes and must not depend on order.
        HashSet<String> a = new HashSet<>(Arrays.asList("x", "y", "z"));
        HashSet<String> b = new HashSet<>(Arrays.asList("z", "y", "x"));
        p("hashCode order independent", a.hashCode() == b.hashCode());
        p("hashCode equals Set contract",
          a.hashCode() == ("x".hashCode() + "y".hashCode() + "z".hashCode()));
        p("empty hashCode", new HashSet<String>().hashCode());
        p("equals same contents", a.equals(b));
        p("equals different contents", a.equals(new HashSet<>(Arrays.asList("x"))));

        Iterator<String> it = a.iterator();
        p("iterator hasNext", it.hasNext());
        int n = 0;
        while (it.hasNext()) { it.next(); n++; }
        p("iterator walked size", n);
        p("iterator exhausted hasNext", it.hasNext());
        t("iterator next past end", () -> exhaust(a.iterator()));
        // iterator().remove() must write through
        HashSet<String> r = new HashSet<>(Arrays.asList("m", "n"));
        Iterator<String> ri = r.iterator();
        ri.next(); ri.remove();
        p("iterator.remove size", r.size());
        t("iterator.remove twice", () -> { Iterator<String> q = r.iterator(); q.next(); q.remove(); q.remove(); });
        // a modification during iteration must be a ConcurrentModificationException
        t("CME on concurrent add", () -> {
            HashSet<String> z = new HashSet<>(Arrays.asList("1", "2", "3"));
            for (String e : z) z.add("4");
        });
    }
    /** Walks to the end and asks for one more: NoSuchElementException. */
    static void exhaust(Iterator<String> it) {
        while (it.hasNext()) it.next();
        it.next();
    }

    public static void main(String[] args) {
        byteCopies();
        objectCopies();
        equalsHashFill();
        hashSet();
        System.out.println("DONE ArraysHashSetShadowSweep");
    }
}
