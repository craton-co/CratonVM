// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 23, lane fuzz9: MethodHandles.arrayElementVarHandle for
// reference arrays other than Object[] -- String[], Number[], CharSequence[],
// Object[][], String[][] -- each given its own array type and a WIDER handle's
// array (String[] through the Object[] handle, Integer[] through Number[],
// StringBuilder[] through CharSequence[], String[][] through Object[][]) and a
// NARROWER handle's array (Object[] through String[], Object[][] through
// String[][]). Access modes: set, setVolatile, setRelease, setOpaque,
// compareAndSet, compareAndExchange, getAndSet, get, and the cast contexts
// `(String) VH.compareAndExchange(..)` / `(String) VH.getAndSet(..)`.
// Cases per (handle, array, mode): a good value at index 0, a wrong-typed
// value at index 1, null at index 3, a good value at index 4 (out of bounds),
// a wrong value at index -1 (both bad), a wrong-typed EXPECTED value (CAS
// forms only), and a null array (own array type only).
//
// Risk targeted: wave 22's opt-in vh-const Object[] stores
// (CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE) and the interpreter door
// vh_array_store_refusal, now for non-Object[] handles and for nested array
// component types; check order (coordinate cast, index, expected, value).
//
// Every row is printed cold (interpreted), compared again after each call site
// was warmed 25000 times on good values ("warm" lines print only rows that
// differ), and then all rows are re-run 32 times hot comparing exception CLASS
// and array contents only (messages of implicit exceptions in C2 code become
// null under OmitStackTraceInFastThrow, so hot rows never compare messages).
// NullPointerException messages are never printed (helpful text names JDK
// locals).
//
// Run:
//   javac -d out R21Fz9VhArrRef1.java
//   java -XX:-OmitStackTraceInFastThrow -Xmx256m -cp out R21Fz9VhArrRef1   # HotSpot 25
//   (the flag keeps HotSpot's C2 from replacing implicit exceptions -- aastore,
//   array index, checkcast -- by preallocated ones with null messages, which
//   would make the "warm" message lines depend on trap counts)
//   cratonvm -Xmx256m -cp out R21Fz9VhArrRef1             # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz9VhArrRef1
//   cratonvm --nojit -Xmx256m -cp out R21Fz9VhArrRef1
//   CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE=1 cratonvm -Xmx256m -cp out R21Fz9VhArrRef1
//
// HotSpot 25 expectation (by reading JDK 25 X-VarHandle.java.template
// VarHandleReferences.Array and VarHandleGuards; not yet run):
//   * The call sites are (Object, int, Object..) so they match the erased
//     access-mode type and go straight to the Array body (no asType casts):
//     coordinate `arrayType.cast(oarray)` first -> narrower array is
//     "ClassCastException msg=Cannot cast [Ljava.lang.Object; to [Ljava.lang.String;"
//     (and "[[Ljava.lang.Object; to [[Ljava.lang.String;") on every mode/case
//     except the null-array case.
//   * Same-type array, wrong value: componentType.cast -> CCE
//     "Cannot cast java.lang.Integer to java.lang.String" (Number: "java.lang.String
//     to java.lang.Number"; CharSequence: "java.lang.Integer to
//     java.lang.CharSequence"; Object[][]: "java.lang.String to [Ljava.lang.Object;";
//     String[][]: "[Ljava.lang.Object; to [Ljava.lang.String;").
//   * Wider handle, wrong value: reflectiveTypeCheck -> "ArrayStoreException
//     msg=null" for setVolatile/setRelease/setOpaque/CAS/CAE/getAndSet. For
//     plain `set` the message depends on whether the JDK 25 body is
//     `array[index] = runtimeTypeCheck(..)` (msg=null) or a plain aastore
//     (msg=<value class name>, which R15Vh7ArrayCastOrder row aseIn measured as
//     "java.lang.Integer"); the header of R21VhConstRefRows says msg=null. HotSpot's
//     line decides; this probe prints it.
//   * Index checks: "ArrayIndexOutOfBoundsException msg=Index 4 out of bounds for
//     length 4" / "Index -1 out of bounds for length 4". Both-bad (index -1,
//     wrong value): the Preconditions.checkIndex modes give the AIOOBE (index
//     evaluated before the value); `set` gives whatever its RHS-first order gives.
//   * Wrong EXPECTED: same-type handle -> CCE on the expected (array unchanged);
//     wider handle (componentType Object/Number/CharSequence) -> only compared,
//     a miss, array unchanged.
//   * Cast contexts: the exchange happens, THEN the witness is cast
//     (VarHandleGuards: ad.returnType.cast): a CCE row still shows the new
//     element in arr=.
//   * Null array: NullPointerException.
//   * "warm same=<rows>/<rows>", "hot same=<32*rows>/<32*rows>".

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.List;

public class R21Fz9VhArrRef1 {
    static final VarHandle VO = MethodHandles.arrayElementVarHandle(Object[].class);
    static final VarHandle VS = MethodHandles.arrayElementVarHandle(String[].class);
    static final VarHandle VN = MethodHandles.arrayElementVarHandle(Number[].class);
    static final VarHandle VC = MethodHandles.arrayElementVarHandle(CharSequence[].class);
    static final VarHandle VOO = MethodHandles.arrayElementVarHandle(Object[][].class);
    static final VarHandle VSS = MethodHandles.arrayElementVarHandle(String[][].class);

    static final String[] HN = {"VO", "VS", "VN", "VC", "VOO", "VSS"};
    static final String[] MN = {"set", "setVolatile", "setRelease", "setOpaque", "cas", "cae", "gas",
        "castCae", "castGas", "get"};
    static final int MODES = MN.length;

    // ---- one method per handle, one constant call site per mode ----

    static Object opO(int m, Object a, int i, Object e, Object v) {
        switch (m) {
            case 0: VO.set(a, i, v); return "void";
            case 1: VO.setVolatile(a, i, v); return "void";
            case 2: VO.setRelease(a, i, v); return "void";
            case 3: VO.setOpaque(a, i, v); return "void";
            case 4: return VO.compareAndSet(a, i, e, v);
            case 5: return VO.compareAndExchange(a, i, e, v);
            case 6: return VO.getAndSet(a, i, v);
            case 7: return (String) VO.compareAndExchange(a, i, e, v);
            case 8: return (String) VO.getAndSet(a, i, v);
            default: return VO.get(a, i);
        }
    }

    static Object opS(int m, Object a, int i, Object e, Object v) {
        switch (m) {
            case 0: VS.set(a, i, v); return "void";
            case 1: VS.setVolatile(a, i, v); return "void";
            case 2: VS.setRelease(a, i, v); return "void";
            case 3: VS.setOpaque(a, i, v); return "void";
            case 4: return VS.compareAndSet(a, i, e, v);
            case 5: return VS.compareAndExchange(a, i, e, v);
            case 6: return VS.getAndSet(a, i, v);
            case 7: return (String) VS.compareAndExchange(a, i, e, v);
            case 8: return (String) VS.getAndSet(a, i, v);
            default: return VS.get(a, i);
        }
    }

    static Object opN(int m, Object a, int i, Object e, Object v) {
        switch (m) {
            case 0: VN.set(a, i, v); return "void";
            case 1: VN.setVolatile(a, i, v); return "void";
            case 2: VN.setRelease(a, i, v); return "void";
            case 3: VN.setOpaque(a, i, v); return "void";
            case 4: return VN.compareAndSet(a, i, e, v);
            case 5: return VN.compareAndExchange(a, i, e, v);
            case 6: return VN.getAndSet(a, i, v);
            case 7: return (String) VN.compareAndExchange(a, i, e, v);
            case 8: return (String) VN.getAndSet(a, i, v);
            default: return VN.get(a, i);
        }
    }

    static Object opC(int m, Object a, int i, Object e, Object v) {
        switch (m) {
            case 0: VC.set(a, i, v); return "void";
            case 1: VC.setVolatile(a, i, v); return "void";
            case 2: VC.setRelease(a, i, v); return "void";
            case 3: VC.setOpaque(a, i, v); return "void";
            case 4: return VC.compareAndSet(a, i, e, v);
            case 5: return VC.compareAndExchange(a, i, e, v);
            case 6: return VC.getAndSet(a, i, v);
            case 7: return (String) VC.compareAndExchange(a, i, e, v);
            case 8: return (String) VC.getAndSet(a, i, v);
            default: return VC.get(a, i);
        }
    }

    static Object opOO(int m, Object a, int i, Object e, Object v) {
        switch (m) {
            case 0: VOO.set(a, i, v); return "void";
            case 1: VOO.setVolatile(a, i, v); return "void";
            case 2: VOO.setRelease(a, i, v); return "void";
            case 3: VOO.setOpaque(a, i, v); return "void";
            case 4: return VOO.compareAndSet(a, i, e, v);
            case 5: return VOO.compareAndExchange(a, i, e, v);
            case 6: return VOO.getAndSet(a, i, v);
            case 7: return (String) VOO.compareAndExchange(a, i, e, v);
            case 8: return (String) VOO.getAndSet(a, i, v);
            default: return VOO.get(a, i);
        }
    }

    static Object opSS(int m, Object a, int i, Object e, Object v) {
        switch (m) {
            case 0: VSS.set(a, i, v); return "void";
            case 1: VSS.setVolatile(a, i, v); return "void";
            case 2: VSS.setRelease(a, i, v); return "void";
            case 3: VSS.setOpaque(a, i, v); return "void";
            case 4: return VSS.compareAndSet(a, i, e, v);
            case 5: return VSS.compareAndExchange(a, i, e, v);
            case 6: return VSS.getAndSet(a, i, v);
            case 7: return (String) VSS.compareAndExchange(a, i, e, v);
            case 8: return (String) VSS.getAndSet(a, i, v);
            default: return VSS.get(a, i);
        }
    }

    static Object op(int h, int m, Object a, int i, Object e, Object v) {
        switch (h) {
            case 0: return opO(m, a, i, e, v);
            case 1: return opS(m, a, i, e, v);
            case 2: return opN(m, a, i, e, v);
            case 3: return opC(m, a, i, e, v);
            case 4: return opOO(m, a, i, e, v);
            default: return opSS(m, a, i, e, v);
        }
    }

    // ---- arrays ----
    // O: Object[]  S: String[]  N: Number[]  I: Integer[]  C: CharSequence[]
    // B: StringBuilder[]  OO: Object[][]  SS: String[][]   (all length 4)

    static Object mk(String kind) {
        switch (kind) {
            case "O": return new Object[] {"a", "b", null, 7};
            case "S": return new String[] {"a", "b", null, "d"};
            case "N": return new Number[] {1, 2L, null, 4};
            case "I": return new Integer[] {1, 2, null, 4};
            case "C": return new CharSequence[] {"a", "b", null, "d"};
            case "B": return new StringBuilder[] {new StringBuilder("a"), new StringBuilder("b"), null,
                new StringBuilder("d")};
            case "OO": return new Object[][] {{"x"}, new String[] {"y"}, null, {}};
            case "SS": return new String[][] {{"x"}, {"y"}, null, {}};
            default: throw new AssertionError(kind);
        }
    }

    static String r(Object o) {
        if (o == null) {
            return "null";
        }
        if (o instanceof String) {
            return "\"" + o + "\"";
        }
        if (o instanceof Object[] oa) {
            return o.getClass().getComponentType().getSimpleName() + "[" + oa.length + "]";
        }
        return o.getClass().getSimpleName() + ":" + o;
    }

    static String arr(Object a) {
        if (!(a instanceof Object[] oa)) {
            return String.valueOf(a);
        }
        StringBuilder sb = new StringBuilder("{");
        for (int k = 0; k < oa.length; k++) {
            sb.append(k == 0 ? "" : ",").append(r(oa[k]));
        }
        return sb.append('}').toString();
    }

    // ---- rows ----

    record Pair(int h, String kind, Object good, Object wrong) {
    }

    record Row(int h, int m, String kind, String c, int idx, Object e, Object v) {
        String key() {
            return HN[h] + " " + MN[m] + " " + (kind == null ? "null" : kind) + " " + c;
        }
    }

    static final Integer NINE = 9;
    static final Long NINE_L = 9L;
    static final String X = "x";

    static List<Pair> pairs() {
        List<Pair> p = new ArrayList<>();
        p.add(new Pair(0, "O", X, NINE));
        p.add(new Pair(0, "S", X, NINE));
        p.add(new Pair(1, "S", X, NINE));
        p.add(new Pair(1, "O", X, NINE));
        p.add(new Pair(2, "N", NINE_L, X));
        p.add(new Pair(2, "I", NINE, NINE_L));
        p.add(new Pair(3, "C", X, NINE));
        p.add(new Pair(3, "B", new StringBuilder("q"), X));
        p.add(new Pair(4, "OO", new String[] {"z"}, X));
        p.add(new Pair(4, "SS", new String[] {"z"}, new Object[] {"z"}));
        p.add(new Pair(5, "SS", new String[] {"z"}, new Object[] {"z"}));
        p.add(new Pair(5, "OO", new String[] {"z"}, new Object[] {"z"}));
        return p;
    }

    static final String[] OWN = {"O", "S", "N", "C", "OO", "SS"};

    static List<Row> rows() {
        List<Row> rows = new ArrayList<>();
        for (Pair p : pairs()) {
            Object[] fresh = (Object[]) mk(p.kind);
            for (int m = 0; m < MODES; m++) {
                rows.add(new Row(p.h, m, p.kind, "good@0", 0, fresh[0], p.good));
                rows.add(new Row(p.h, m, p.kind, "wrong@1", 1, fresh[1], p.wrong));
                rows.add(new Row(p.h, m, p.kind, "null@3", 3, fresh[3], null));
                rows.add(new Row(p.h, m, p.kind, "good@4", 4, null, p.good));
                rows.add(new Row(p.h, m, p.kind, "wrong@-1", -1, null, p.wrong));
                if (m == 4 || m == 5 || m == 7) {
                    rows.add(new Row(p.h, m, p.kind, "wrongExp@1", 1, p.wrong, p.good));
                }
            }
        }
        for (int h = 0; h < 6; h++) {
            Object good = pairs().get(h * 2).good;
            for (int m = 0; m < MODES; m++) {
                rows.add(new Row(h, m, null, "nullArr", 0, null, good));
            }
        }
        return rows;
    }

    // Expected values are re-read from a fresh array of the same kind, so the
    // CAS hit rows compare against the same (interned / cached) instance.
    static Object expectedFor(Row row, Object a) {
        if (row.e == null || a == null || row.c.startsWith("wrongExp")) {
            return row.e;
        }
        return ((Object[]) a)[row.idx];
    }

    static String exc(Throwable t, boolean msg) {
        if (t instanceof NullPointerException) {
            return "EXC java.lang.NullPointerException";
        }
        return "EXC " + t.getClass().getName() + (msg ? " msg=" + t.getMessage() : "");
    }

    static String run(Row row, boolean msg) {
        Object a = row.kind == null ? null : mk(row.kind);
        String out;
        try {
            out = "ok " + r(op(row.h, row.m, a, row.idx, expectedFor(row, a), row.v));
        } catch (Throwable t) {
            out = exc(t, msg);
        }
        return out + " arr=" + arr(a);
    }

    static void warm() {
        Object[][] own = new Object[6][];
        for (int h = 0; h < 6; h++) {
            own[h] = (Object[]) mk(OWN[h]);
        }
        Object[] goods = {X, X, NINE_L, X, new String[] {"z"}, new String[] {"z"}};
        long sink = 0;
        for (int k = 0; k < 25_000; k++) {
            int i = k & 1;
            for (int h = 0; h < 6; h++) {
                Object[] a = own[h];
                for (int m = 0; m < MODES; m++) {
                    if ((m == 7 || m == 8) && (h == 2 || h == 4 || h == 5)) {
                        a[i] = null; // the String cast must not fail while warming
                    }
                    Object res = op(h, m, a, i, a[i], m == 7 || m == 8 ? null : goods[h]);
                    if (res != null) {
                        sink++;
                    }
                }
                a[0] = goods[h];
                a[1] = goods[h];
            }
        }
        System.out.println("warmed sink>0=" + (sink > 0));
    }

    public static void main(String[] args) {
        List<Row> rows = rows();
        List<String> cold = new ArrayList<>();
        for (Row row : rows) {
            String s = run(row, true);
            cold.add(s);
            System.out.println("cold " + row.key() + " -> " + s);
        }
        warm();
        int same = 0;
        for (int k = 0; k < rows.size(); k++) {
            String s = run(rows.get(k), true);
            if (s.equals(cold.get(k))) {
                same++;
            } else {
                System.out.println("warm differs " + rows.get(k).key() + " -> " + s);
            }
        }
        System.out.println("warm same=" + same + "/" + rows.size());
        List<String> coldNoMsg = new ArrayList<>();
        for (String c : cold) {
            coldNoMsg.add(stripMsg(c));
        }
        int hot = 0;
        int total = 0;
        for (int round = 0; round < 32; round++) {
            for (int k = 0; k < rows.size(); k++) {
                String s = run(rows.get(k), false);
                total++;
                if (s.equals(coldNoMsg.get(k))) {
                    hot++;
                } else if (round == 31) {
                    System.out.println("hot differs " + rows.get(k).key() + " -> " + s);
                }
            }
        }
        System.out.println("hot same=" + hot + "/" + total);
    }

    static String stripMsg(String s) {
        int m = s.indexOf(" msg=");
        if (m < 0) {
            return s;
        }
        int a = s.lastIndexOf(" arr=");
        return s.substring(0, m) + s.substring(a);
    }
}
