package cratonvm;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Differential exercise program for the record `hashCode`/`equals` interpreter
 * intrinsics (`InterpIntrinsic::RecordHashCode` / `RecordEquals`, JEP 395).
 *
 * Every `r:` line must be byte-for-byte identical with intrinsics ON and with
 * `CRATONVM_DISABLE_INTRINSICS=1`, which forces the same calls back through
 * the `invokedynamic ObjectMethods.bootstrap` call-site path.
 *
 * Raw `hashCode()` values are deliberately NOT printed: the JLS leaves a
 * record's hash unspecified, so a value assertion would pin an implementation
 * detail. What IS printed is every observable consequence of it — equal
 * objects hashing alike, bucket placement in `HashMap`/`HashSet`, and
 * iteration-order-independent lookups — plus `equals`/`toString` results,
 * which the JLS does specify.
 */
public class IntrinsicRecordDiff {

    enum Kind { INSERT, UPDATE, DELETE }

    record Prim(int i, long l, boolean b, char c, byte by, short sh, float f, double d) {}
    record Str(String a, String b) {}
    record Enm(Kind k, int n) {}
    record Shape(String table, Kind kind, int shapeHash) {}
    record Group(String table, Kind kind, Shape shape, List<String> ops, boolean pre, int ordinal) {}
    record Node(Group group, long stableId) {}
    record Nullable(String s, Object o) {}
    record Arr(int[] a, Object[] b) {}
    record Coll(List<String> items, Map<String, Integer> m) {}
    record Boxed(Integer i, Long l, Double d) {}
    record Empty() {}

    /** Hand-written bodies: the intrinsic must never divert these. */
    record Custom(int a, int b) {
        @Override public int hashCode() { return 4242; }
        @Override public boolean equals(Object o) { return o instanceof Custom c && c.a == a; }
        @Override public String toString() { return "CUSTOM"; }
    }

    static final StringBuilder OUT = new StringBuilder();

    static void r(String label, Object value) {
        OUT.append("r: ").append(label).append('=').append(value).append('\n');
    }

    /** Everything observable about a pair, without printing the raw hash. */
    static void pair(String label, Object x, Object y) {
        r(label + ".equals", x.equals(y));
        r(label + ".equalsSym", y.equals(x));
        r(label + ".hashAgrees", x.hashCode() == y.hashCode());
        r(label + ".reflexive", x.equals(x));
        r(label + ".vsNull", x.equals(null));
        r(label + ".vsOtherType", x.equals("not-a-record"));
        r(label + ".hashStable", x.hashCode() == x.hashCode());
        Set<Object> set = new HashSet<>();
        set.add(x);
        set.add(y);
        r(label + ".setSize", set.size());
        r(label + ".setContainsX", set.contains(x));
        r(label + ".setContainsY", set.contains(y));
        Map<Object, String> map = new HashMap<>();
        map.put(x, "first");
        map.put(y, "second");
        r(label + ".mapSize", map.size());
        r(label + ".mapGetX", map.get(x));
        r(label + ".mapGetY", map.get(y));
    }

    static List<String> ops(int n) {
        List<String> list = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            list.add("op-" + i);
        }
        return list;
    }

    public static void main(String[] args) {
        // --- all-primitive components -----------------------------------
        pair("prim.same",
             new Prim(1, 2L, true, 'x', (byte) 3, (short) 4, 1.5f, 2.5d),
             new Prim(1, 2L, true, 'x', (byte) 3, (short) 4, 1.5f, 2.5d));
        pair("prim.boolDiff",
             new Prim(1, 2L, true, 'x', (byte) 3, (short) 4, 1.5f, 2.5d),
             new Prim(1, 2L, false, 'x', (byte) 3, (short) 4, 1.5f, 2.5d));
        pair("prim.charDiff",
             new Prim(1, 2L, true, 'x', (byte) 3, (short) 4, 1.5f, 2.5d),
             new Prim(1, 2L, true, 'y', (byte) 3, (short) 4, 1.5f, 2.5d));
        pair("prim.longHighWord",
             new Prim(0, 1L << 32, false, 'a', (byte) 0, (short) 0, 0f, 0d),
             new Prim(0, 1L, false, 'a', (byte) 0, (short) 0, 0f, 0d));
        // Float/Double.equals bit semantics: NaN == NaN, +0.0 != -0.0.
        pair("prim.nan",
             new Prim(0, 0, false, 'a', (byte) 0, (short) 0, Float.NaN, Double.NaN),
             new Prim(0, 0, false, 'a', (byte) 0, (short) 0, Float.NaN, Double.NaN));
        pair("prim.signedZero",
             new Prim(0, 0, false, 'a', (byte) 0, (short) 0, 0.0f, 0.0d),
             new Prim(0, 0, false, 'a', (byte) 0, (short) 0, 0.0f, -0.0d));
        r("prim.toString",
          new Prim(1, 2L, true, 'x', (byte) 3, (short) 4, 1.5f, 2.5d).toString());

        // --- String components (distinct-but-equal instances) -----------
        pair("str.same", new Str(new String("employee"), "person"), new Str("employee", "person"));
        pair("str.diff", new Str("employee", "person"), new Str("employee", "persons"));
        pair("str.utf16", new Str("é中", "Ł"), new Str("é中", "Ł"));
        pair("str.utf16Diff", new Str("é中", "Ł"), new Str("é中", "ł"));
        pair("str.empty", new Str("", ""), new Str("", ""));
        r("str.toString", new Str("a", "b").toString());

        // --- enum components (identity hashCode/equals, final in Enum) --
        pair("enm.same", new Enm(Kind.UPDATE, 3), new Enm(Kind.UPDATE, 3));
        pair("enm.diff", new Enm(Kind.UPDATE, 3), new Enm(Kind.DELETE, 3));
        r("enm.toString", new Enm(Kind.INSERT, 1).toString());

        // --- nested records, the Hibernate graph-planner key shape ------
        Node n1 = new Node(new Group("employee", Kind.INSERT,
                                     new Shape("employee", Kind.INSERT, 17),
                                     ops(8), true, 3), 42L);
        Node n2 = new Node(new Group("employee", Kind.INSERT,
                                     new Shape("employee", Kind.INSERT, 17),
                                     ops(8), true, 3), 42L);
        Node n3 = new Node(new Group("employee", Kind.INSERT,
                                     new Shape("employee", Kind.INSERT, 18),
                                     ops(8), true, 3), 42L);
        pair("node.same", n1, n2);
        pair("node.deepDiff", n1, n3);
        r("node.toString", n1.toString());

        // --- null components -------------------------------------------
        pair("null.both", new Nullable(null, null), new Nullable(null, null));
        pair("null.leftNull", new Nullable(null, null), new Nullable("x", null));
        pair("null.rightNull", new Nullable("x", null), new Nullable(null, null));
        r("null.toString", new Nullable(null, null).toString());

        // --- array components use identity semantics --------------------
        int[] ints = {1, 2, 3};
        Object[] objs = {"a"};
        pair("arr.sameInstances", new Arr(ints, objs), new Arr(ints, objs));
        pair("arr.equalContent",
             new Arr(new int[] {1, 2, 3}, new Object[] {"a"}),
             new Arr(new int[] {1, 2, 3}, new Object[] {"a"}));
        // A record-typed array must never compare equal to a record instance:
        // a reference array reports its COMPONENT class id.
        Str[] strArray = {new Str("q", "r")};
        r("arr.recordVsArray", new Str("q", "r").equals(strArray));
        r("arr.arrayVsRecord", ((Object) strArray).equals(new Str("q", "r")));
        // A record holding a record array.
        Object[] holder = {new Str("q", "r")};
        pair("arr.recordInArray", new Arr(ints, holder), new Arr(ints, holder));

        // --- collection components delegate to the collection -----------
        pair("coll.same",
             new Coll(List.of("a", "b"), Map.of("k", 1)),
             new Coll(new ArrayList<>(List.of("a", "b")), new LinkedHashMap<>(Map.of("k", 1))));
        pair("coll.diff", new Coll(List.of("a", "b"), Map.of("k", 1)),
                          new Coll(List.of("a"), Map.of("k", 1)));

        // --- boxed components use wrapper value equality ----------------
        pair("boxed.same", new Boxed(100000, 100000L, 1.5), new Boxed(100000, 100000L, 1.5));
        pair("boxed.diff", new Boxed(100000, 100000L, 1.5), new Boxed(100001, 100000L, 1.5));
        pair("boxed.nullBox", new Boxed(null, null, null), new Boxed(null, null, null));

        // --- a component-less record ------------------------------------
        pair("empty.same", new Empty(), new Empty());
        r("empty.toString", new Empty().toString());

        // --- hand-written overrides must be honoured --------------------
        r("custom.hashIs4242", new Custom(1, 2).hashCode() == 4242);
        r("custom.equalsIgnoresB", new Custom(1, 9).equals(new Custom(1, 8)));
        r("custom.equalsChecksA", new Custom(2, 9).equals(new Custom(1, 9)));
        r("custom.toString", new Custom(1, 2).toString());

        // --- volume: exercise the steady-state IC, not just the fill ----
        Map<Node, String> big = new HashMap<>();
        for (int i = 0; i < 300; i++) {
            big.put(new Node(new Group("t" + i, Kind.values()[i % 3],
                                       new Shape("t" + i, Kind.values()[i % 3], i),
                                       ops(i % 5), i % 2 == 0, i), i), "v" + i);
        }
        r("big.size", big.size());
        int hits = 0;
        int misses = 0;
        for (int i = 0; i < 300; i++) {
            String v = big.get(new Node(new Group("t" + i, Kind.values()[i % 3],
                                                  new Shape("t" + i, Kind.values()[i % 3], i),
                                                  ops(i % 5), i % 2 == 0, i), i));
            if (("v" + i).equals(v)) {
                hits++;
            } else {
                misses++;
            }
        }
        r("big.hits", hits);
        r("big.misses", misses);
        // Absent keys must miss.
        r("big.absent", big.get(new Node(new Group("nope", Kind.INSERT,
                                                  new Shape("nope", Kind.INSERT, 0),
                                                  ops(1), false, 0), 0)));

        // --- megamorphic site: one call site, many record classes -------
        Object[] mixed = {
            new Prim(1, 1, true, 'a', (byte) 1, (short) 1, 1f, 1d),
            new Str("a", "b"), new Enm(Kind.INSERT, 1), new Nullable(null, null),
            new Empty(), new Custom(1, 2), n1, "plain-string", Integer.valueOf(7),
        };
        StringBuilder mega = new StringBuilder();
        for (int round = 0; round < 3; round++) {
            for (Object left : mixed) {
                for (Object right : mixed) {
                    mega.append(left.equals(right) ? '1' : '0');
                }
                mega.append(left.hashCode() == left.hashCode() ? 's' : '!');
            }
        }
        r("mega.matrix", mega.toString());

        System.out.print(OUT);
        System.out.println("INTRINSIC_RECORD_DIFF_OK " + big.size());
    }
}
