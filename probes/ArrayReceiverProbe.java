import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.HashSet;
import java.util.IdentityHashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.TreeMap;

/**
 * Every door that takes an arbitrary Object and probes its SHAPE, handed an
 * ARRAY.
 *
 * An array is the one receiver for which "how many fields does it have?" lies:
 * this VM mirrors an array's LENGTH into its `num_slots`, so a `byte[23]` reports
 * 23 fields and walks straight through a `num_fields(obj) < 2` guard into a
 * positional `get_field(obj, 0)` / `get_field(obj, 1)` read. Those reads address
 * a 16-byte tagged `Value` cell at `HEADER_SIZE + i * 16`, while the array's body
 * holds packed elements (1 byte for `byte[]`, 8 for a reference) — so the read
 * decodes unrelated element bytes as a `(tag, payload)` pair and, past the first
 * couple of indices, leaves the allocation entirely.
 *
 * That is the producer species behind two records:
 *   * corrupt-value-cell-producer-was-a-string-array-FIXED-20260822 — a
 *     `String[]` matching a `class_name == "java/lang/String"` fast path,
 *     because a reference array carries its COMPONENT's class id;
 *   * corrupt-value-cell-one-unreproduced-hit-in-kafkametrics-20260822 — a cell
 *     whose `raw0` decoded as the ASCII text `"t/Proxy\0"`, i.e. the tail of the
 *     class name `java/lang/reflect/Proxy` read out of a `byte[]` body.
 *
 * Nothing here asserts a VM internal. Every row is a HotSpot-observable answer,
 * and every array rendering is reduced to its SHAPE (the `[Lfoo;@` prefix) so no
 * row depends on an identity hash. Run it under
 * `CRATONVM_DBG_CORRUPT_CELL=1` and the `[corrupt-cell] array_receiver=N` line
 * at exit says whether any door still strided an array as an object.
 */
public class ArrayReceiverProbe {

    /** Reduce a rendering to its shape: `[Ljava.lang.String;@<hash>` -> `[Ljava.lang.String;@`. */
    static String shape(Object o) {
        if (o == null) {
            return "null";
        }
        String s = String.valueOf(o);
        int at = s.lastIndexOf('@');
        return at < 0 ? s : s.substring(0, at + 1);
    }

    interface Op { Object run() throws Throwable; }

    static void row(String what, Op op) {
        try {
            System.out.println(what + "\t" + shape(op.run()));
        } catch (Throwable t) {
            System.out.println(what + "\t" + t.getClass().getName());
        }
    }

    /** A byte[] whose text is exactly the class name the KafkaMetrics cell decoded. */
    static final byte[] PROXY_NAME = "java/lang/reflect/Proxy".getBytes();
    static final char[] PROXY_CHARS = "java/lang/reflect/Proxy".toCharArray();
    static final String[] REFS = { "y", "n" };
    static final Object[] OBJS = { "y", "n" };
    static final int[] INTS = { 1, 2, 3, 4, 5, 6, 7, 8 };
    static final String[][] NESTED = { { "a" }, { "b" } };
    static final byte[] EMPTY = {};
    static final byte[] ONE = { 7 };

    static final Object[] RECEIVERS = {
        PROXY_NAME, PROXY_CHARS, REFS, OBJS, INTS, NESTED, EMPTY, ONE,
    };

    static String name(Object o) {
        return o.getClass().getName();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("== rendering doors ==");
        for (Object r : RECEIVERS) {
            row("String.valueOf(Object) " + name(r), () -> String.valueOf(r));
            row("sb.append(Object) " + name(r), () -> new StringBuilder().append(r).toString());
            row("sb.append(CharSequence?) " + name(r),
                    () -> new StringBuilder().append((Object) r).toString());
            row("sb.insert(0,Object) " + name(r),
                    () -> new StringBuilder("x").insert(0, r).toString());
            row("Objects.toString " + name(r), () -> Objects.toString(r));
            row("String.concat-via-+ " + name(r), () -> "" + r);
            row("r.toString() " + name(r), () -> r.toString());
        }

        System.out.println("== identity doors ==");
        for (Object r : RECEIVERS) {
            // Not the VALUE of the hash (host-dependent) — only that the two
            // agree, which is the property a strided read breaks.
            row("hashCode==identityHashCode " + name(r),
                    () -> r.hashCode() == System.identityHashCode(r));
            row("equals(self) " + name(r), () -> r.equals(r));
            row("equals(copy-of-self) " + name(r), () -> r.equals(r.clone()));
        }

        System.out.println("== collection doors ==");
        for (Object r : RECEIVERS) {
            row("HashMap.put/get " + name(r), () -> {
                Map<Object, Object> m = new HashMap<>();
                m.put(r, "v");
                return m.get(r);
            });
            row("LinkedHashMap.put/get " + name(r), () -> {
                Map<Object, Object> m = new LinkedHashMap<>();
                m.put(r, "v");
                return m.get(r);
            });
            row("IdentityHashMap.put/get " + name(r), () -> {
                Map<Object, Object> m = new IdentityHashMap<>();
                m.put(r, "v");
                return m.get(r);
            });
            row("HashSet.add/contains " + name(r), () -> {
                HashSet<Object> s = new HashSet<>();
                s.add(r);
                return s.contains(r);
            });
            row("List.indexOf " + name(r), () -> {
                List<Object> l = new ArrayList<>();
                l.add(r);
                return l.indexOf(r);
            });
            row("TreeMap.put(String key, array value) " + name(r), () -> {
                Map<String, Object> m = new TreeMap<>();
                m.put("k", r);
                return shape(m.get("k"));
            });
        }

        System.out.println("== Arrays helpers ==");
        row("Arrays.toString(byte[])", () -> Arrays.toString(PROXY_NAME));
        row("Arrays.toString(String[])", () -> Arrays.toString(REFS));
        row("Arrays.deepToString(String[][])", () -> Arrays.deepToString(NESTED));
        row("Arrays.asList(String[]).toString", () -> Arrays.asList(REFS).toString());
        row("Arrays.hashCode(byte[])", () -> Arrays.hashCode(PROXY_NAME));
        row("Arrays.equals(byte[],copy)", () -> Arrays.equals(PROXY_NAME, PROXY_NAME.clone()));
        row("new String(byte[])", () -> new String(PROXY_NAME));
        row("new String(char[])", () -> new String(PROXY_CHARS));

        System.out.println("== the two recorded producers, directly ==");
        // The `String[]` one: all four doors must AGREE, which is what a
        // per-door fast path regressing breaks where four matching wrong
        // answers would not.
        String a = String.valueOf((Object) REFS);
        String b = new StringBuilder().append((Object) REFS).toString();
        String c = new StringBuilder().append(REFS.getClass().getName()).toString();
        System.out.println("valueOf==append\t" + shape(a).equals(shape(b)));
        System.out.println("valueOf shape\t" + shape(a));
        System.out.println("componentClassName\t" + c);
        // The `byte[]` one: the receiver whose body IS the class-name text.
        System.out.println("proxyName decoded\t" + new String(PROXY_NAME));
        System.out.println("proxyName rendered\t" + shape(PROXY_NAME));
        System.out.println("proxyName length\t" + PROXY_NAME.length);
        // Index 1 of a 16-byte-cell stride lands at byte offset 16 of the body,
        // which for this array is the "t/Proxy" tail — the exact bytes the
        // KafkaMetrics cell reported. Read the element the RIGHT way and it is
        // still 't'.
        System.out.println("byte[16]\t" + (char) PROXY_NAME[16]);
        System.out.println("tail\t" + new String(PROXY_NAME, 16, PROXY_NAME.length - 16));
    }
}
