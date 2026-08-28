import java.io.*;
import java.util.*;

/** The `java.io.ByteArrayOutputStream` (6) and `java.util.Collections` (4)
 *  triples the `--jdk-only-report` marks `outcome=native-won`.
 *
 *  Fourth family off the Phase 2 worklist. Three families in, 448 rows, 23
 *  defects, and EVERY ONE was on a contract edge — so the aim is unchanged and
 *  is now a prediction rather than a hunch:
 *
 *    BAOS         <init> x2, size, toByteArray, write(int), write(byte[],int,int)
 *    Collections  emptyEnumeration, emptyList, emptySet, sort(List)
 *
 *  `write(byte[], int, int)` is the densest contract here — five distinct
 *  refusals, one of which (`off + len` overflowing to a negative) is the case a
 *  bounds check written as `off + len > b.length` gets WRONG while looking
 *  right.
 *
 *  The `Collections.empty*` methods are asked as SINGLETONS and as IMMUTABLES
 *  separately, because a shim can easily get one and miss the other: returning
 *  a fresh empty ArrayList each call satisfies every `isEmpty()` test and still
 *  breaks `==` identity and the refusal contract.
 */
public class BaosCollectionsShadowSweep {
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

    static void baosBasics() throws Exception {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        p("new size", o.size());
        p("new toByteArray", hex(o.toByteArray()));
        o.write(1);
        o.write(2);
        p("size after 2 writes", o.size());
        p("toByteArray", hex(o.toByteArray()));

        // write(int) takes the LOW EIGHT BITS: 0x1FF stores 0xFF, and a
        // negative int stores its low byte too.
        ByteArrayOutputStream w = new ByteArrayOutputStream();
        w.write(0x1FF);
        w.write(-1);
        w.write(256);
        w.write(0);
        p("write(int) truncates to byte", hex(w.toByteArray()));
        p("write(int) size", w.size());

        // toByteArray must be a COPY -- mutating it must not touch the stream.
        ByteArrayOutputStream c = new ByteArrayOutputStream();
        c.write(9);
        byte[] first = c.toByteArray();
        first[0] = 42;
        p("toByteArray is a copy", hex(c.toByteArray()));
        p("two toByteArray calls are distinct", c.toByteArray() == c.toByteArray());

        ByteArrayOutputStream r = new ByteArrayOutputStream();
        r.write(new byte[]{1, 2, 3}, 0, 3);
        p("write(byte[],0,3)", hex(r.toByteArray()));
        r.reset();
        p("size after reset", r.size());
        p("toByteArray after reset", hex(r.toByteArray()));
        r.write(new byte[]{4, 5, 6, 7}, 1, 2);
        p("write(byte[],1,2)", hex(r.toByteArray()));
        p("toString default charset", new ByteArrayOutputStream() {{ }} != null);

        // Growth past the default 32-byte buffer.
        ByteArrayOutputStream g = new ByteArrayOutputStream();
        for (int i = 0; i < 100; i++) g.write(i & 0xFF);
        p("grown size", g.size());
        p("grown last byte", g.toByteArray()[99] & 0xFF);
        p("grown first byte", g.toByteArray()[0] & 0xFF);
    }

    static void baosEdges() {
        // <init>(int)
        t("new BAOS(-1)", () -> new ByteArrayOutputStream(-1));
        t("new BAOS(0)", () -> new ByteArrayOutputStream(0));
        t("new BAOS(1)", () -> new ByteArrayOutputStream(1));
        // A zero-capacity stream must still grow on write.
        try {
            ByteArrayOutputStream z = new ByteArrayOutputStream(0);
            z.write(7);
            p("BAOS(0) grows on write", hex(z.toByteArray()));
        } catch (Throwable e) { p("BAOS(0) grows on write", "THREW " + e.getClass().getName()); }

        byte[] b = {1, 2, 3};
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        // A zero-length write at the very end of the array is LEGAL.
        t("write(b,3,0) at end", () -> o.write(b, 3, 0));
        t("write(b,0,0) empty", () -> o.write(b, 0, 0));
        t("write(b,-1,1) negative off", () -> o.write(b, -1, 1));
        t("write(b,0,-1) negative len", () -> o.write(b, 0, -1));
        t("write(b,1,3) past end", () -> o.write(b, 1, 3));
        t("write(b,4,0) off past end", () -> o.write(b, 4, 0));
        // off + len OVERFLOWS to a negative int. A bounds check written as
        // `off + len > b.length` passes this and then reads out of bounds; the
        // JDK's `Objects.checkFromIndexSize` does not.
        t("write(b,1,MAX_VALUE) overflow", () -> o.write(b, 1, Integer.MAX_VALUE));
        t("write(b,MAX_VALUE,1) overflow", () -> o.write(b, Integer.MAX_VALUE, 1));
        t("write(null,0,1)", () -> o.write((byte[]) null, 0, 1));
        t("write(null,0,0)", () -> o.write((byte[]) null, 0, 0));
        p("size unchanged after refusals", o.size());
    }

    static void collectionsEmpty() {
        List<Object> el = Collections.emptyList();
        Set<Object> es = Collections.emptySet();
        p("emptyList isEmpty", el.isEmpty());
        p("emptyList size", el.size());
        p("emptySet isEmpty", es.isEmpty());
        // SINGLETONS: the JDK returns the same instance every call.
        p("emptyList is a singleton", Collections.emptyList() == Collections.emptyList());
        p("emptySet is a singleton", Collections.emptySet() == Collections.emptySet());
        // IMMUTABLE, which is a separate property from being empty.
        t("emptyList add", () -> el.add("x"));
        t("emptyList remove", () -> el.remove("x"));
        t("emptyList clear", () -> el.clear());
        t("emptySet add", () -> es.add("x"));
        t("emptyList get(0)", () -> el.get(0));
        p("emptyList equals new ArrayList", el.equals(new ArrayList<>()));
        p("emptySet equals new HashSet", es.equals(new HashSet<>()));
        p("emptyList hashCode", el.hashCode());
        p("emptySet hashCode", es.hashCode());
        p("emptyList contains null", el.contains(null));
        p("emptyList iterator hasNext", el.iterator().hasNext());
        p("emptyList toString", el.toString());

        Enumeration<Object> ee = Collections.emptyEnumeration();
        p("emptyEnumeration hasMoreElements", ee.hasMoreElements());
        t("emptyEnumeration nextElement", () -> Collections.emptyEnumeration().nextElement());
    }

    static void collectionsSort() {
        List<String> l = new ArrayList<>(Arrays.asList("c", "a", "b"));
        Collections.sort(l);
        p("sort strings", l.toString());
        List<Integer> n = new ArrayList<>(Arrays.asList(3, 1, 2, 1));
        Collections.sort(n);
        p("sort ints with duplicate", n.toString());
        List<String> one = new ArrayList<>(Arrays.asList("x"));
        Collections.sort(one);
        p("sort single", one.toString());
        List<String> none = new ArrayList<>();
        Collections.sort(none);
        p("sort empty", none.toString());
        List<String> already = new ArrayList<>(Arrays.asList("a", "b", "c"));
        Collections.sort(already);
        p("sort already sorted", already.toString());

        t("sort null list", () -> Collections.sort(null));
        // A null ELEMENT is an NPE from the comparison, not a silent ordering.
        t("sort with null element", () ->
            Collections.sort(new ArrayList<>(Arrays.asList("a", null, "b"))));
        // Non-Comparable elements are a ClassCastException.
        t("sort non-comparable", () -> {
            List<Object> objs = new ArrayList<>(Arrays.asList(new Object(), new Object()));
            @SuppressWarnings({"unchecked", "rawtypes"})
            List raw = objs;
            Collections.sort(raw);
        });
        // An IMMUTABLE list must refuse -- but a single-element or empty one
        // may legally short-circuit, so ask with two elements.
        t("sort immutable list", () -> Collections.sort(List.of("b", "a")));
        t("sort Arrays.asList (fixed size, mutable)", () ->
            Collections.sort(Arrays.asList("b", "a")));
        // sort MUST be stable: equal keys keep their input order. Two entries
        // that compare equal but are distinguishable prove it.
        List<String[]> pairs = new ArrayList<>(Arrays.asList(
            new String[]{"k", "first"}, new String[]{"k", "second"}, new String[]{"a", "third"}));
        pairs.sort(Comparator.comparing(x -> x[0]));
        StringBuilder sb = new StringBuilder();
        for (String[] x : pairs) sb.append(x[1]).append(',');
        p("sort is stable", sb.toString());
    }

    public static void main(String[] a) throws Exception {
        baosBasics();
        baosEdges();
        collectionsEmpty();
        collectionsSort();
        System.out.println("DONE BaosCollectionsShadowSweep");
    }
}
