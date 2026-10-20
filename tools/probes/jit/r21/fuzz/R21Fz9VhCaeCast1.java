// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 23, lane fuzz9: VarHandle FIELD compareAndExchange (and
// its Acquire / Release forms, getAndSet for contrast) used in cast contexts
// with wrong-type witnesses: `(String) VO.compareAndExchange(h, e, v)` on an
// Object field, `(Integer) VN.compareAndExchange` on a Number field,
// `(StringBuilder) VCS.compareAndExchange` on a CharSequence field, a static
// field `(String) VSO.compareAndExchange(e, v)`, a primitive cast context
// `(int) VO.compareAndExchange(..)` (asType unbox + widening: Short /
// Character / Integer witnesses, a Long and a null witness), a cast whose
// result is used immediately (`.length()`), an instanceof-pattern context (no
// cast: never a CCE), and the exact-typed String handle given a wrong-typed
// EXPECTED (the cast happens before the exchange). After every row the
// field's value is printed: a witness CCE leaves the field EXCHANGED on a hit
// and unchanged on a miss.
//
// Risk targeted: wave 22's opt-in vh-const cast CAE (CRATONVM_JIT_VH_CONST_CAST_CAE)
// and the interpreter's varhandle_reference_return_refusal, on field shapes
// R21VhConstRefRows does not cover (Number / CharSequence / static fields,
// primitive cast contexts, Acquire / Release forms, holder of the wrong class).
//
// Each row runs cold, then each kernel is warmed 30000 times on good types,
// then each row runs again ("warm"), then each row runs 20000 times hot from a
// freshly reset field, counting outcomes that equal the cold one (exception
// CLASS and field only: no messages in the hot comparison).
//
// Run:
//   javac -d out R21Fz9VhCaeCast1.java
//   java -XX:-OmitStackTraceInFastThrow -Xmx256m -cp out R21Fz9VhCaeCast1   # HotSpot 25
//   (the flag keeps HotSpot's C2 from replacing implicit exceptions -- aastore,
//   array index, checkcast -- by preallocated ones with null messages, which
//   would make the "warm" message lines depend on trap counts)
//   cratonvm -Xmx256m -cp out R21Fz9VhCaeCast1            # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz9VhCaeCast1
//   cratonvm --nojit -Xmx256m -cp out R21Fz9VhCaeCast1
//   CRATONVM_JIT_VH_CONST_CAST_CAE=1 cratonvm -Xmx256m -cp out R21Fz9VhCaeCast1
//
// HotSpot 25 expectation (by reading JDK 25 VarHandleGuards / Invokers /
// ValueConversions and X-VarHandle.java.template; not yet run):
//   * Reference cast contexts whose erased type matches go direct and cast the
//     witness with `ad.returnType.cast(r)`: "CCE msg=Cannot cast
//     java.lang.Integer to java.lang.String", AFTER the exchange.
//       objStr hit-wrong   -> CCE field="n"   (field held 7, expected 7)
//       objStr miss-wrong  -> CCE field=7
//       objStr hit-ok      -> ok "s"  field="n"
//       objStr null-witness-> ok null field="n"
//       numInt hit-wrong (field 2L) -> CCE "Cannot cast java.lang.Long to java.lang.Integer" field=5
//       csSb   hit-wrong (field "s") -> CCE "...java.lang.String to java.lang.StringBuilder" field=sb
//       staticStr: as objStr on the static field
//       acq / rel: as objStr
//       gasStr (getAndSet): CCE, field always replaced
//       lenStr: the same CCE as objStr (cast before .length())
//       pattern: never a CCE, "ok notString" with the field exchanged on a hit
//   * Primitive context `(int)`: the erased return type differs, so the
//     generic asType path unboxes: Integer 7 -> 7, Short 3 -> 3, Character 'A'
//     -> 65, Long -> CCE "Cannot cast java.lang.Long to java.lang.Integer"
//     (ValueConversions.primitiveConversion), String -> CCE (checkcast
//     wording), null -> NPE; the exchange has happened either way.
//   * Exact String handle, wrong EXPECTED (Integer): CCE "Cannot cast
//     java.lang.Integer to java.lang.String" BEFORE the exchange, field
//     unchanged. Wrong holder class: CCE "Cannot cast R21Fz9VhCaeCast1$Other to
//     R21Fz9VhCaeCast1$Holder". Null holder: NullPointerException.
//   * Every "warm" row equals its cold row; every hot row reads same=20000.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.List;

public class R21Fz9VhCaeCast1 {
    static class Holder {
        volatile Object o;
        volatile Number n;
        volatile CharSequence cs;
        volatile String s;
    }

    static class Other {
        volatile Object o;
    }

    static volatile Object SO;

    static final VarHandle VO;
    static final VarHandle VN;
    static final VarHandle VCS;
    static final VarHandle VS;
    static final VarHandle VSO;

    static {
        try {
            MethodHandles.Lookup lk = MethodHandles.lookup();
            VO = lk.findVarHandle(Holder.class, "o", Object.class);
            VN = lk.findVarHandle(Holder.class, "n", Number.class);
            VCS = lk.findVarHandle(Holder.class, "cs", CharSequence.class);
            VS = lk.findVarHandle(Holder.class, "s", String.class);
            VSO = lk.findStaticVarHandle(R21Fz9VhCaeCast1.class, "SO", Object.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // ---- kernels: one constant call site each ----

    static String objStr(Object h, Object e, Object v) {
        return (String) VO.compareAndExchange(h, e, v);
    }

    static String objStrAcq(Object h, Object e, Object v) {
        return (String) VO.compareAndExchangeAcquire(h, e, v);
    }

    static String objStrRel(Object h, Object e, Object v) {
        return (String) VO.compareAndExchangeRelease(h, e, v);
    }

    static String gasStr(Object h, Object v) {
        return (String) VO.getAndSet(h, v);
    }

    static int lenStr(Object h, Object e, Object v) {
        return ((String) VO.compareAndExchange(h, e, v)).length();
    }

    static String pattern(Object h, Object e, Object v) {
        if (VO.compareAndExchange(h, e, v) instanceof String s) {
            return "isString " + s;
        }
        return "notString";
    }

    static int objInt(Object h, Object e, Object v) {
        return (int) VO.compareAndExchange(h, e, v);
    }

    static Integer numInt(Object h, Object e, Object v) {
        return (Integer) VN.compareAndExchange(h, e, v);
    }

    static StringBuilder csSb(Object h, Object e, Object v) {
        return (StringBuilder) VCS.compareAndExchange(h, e, v);
    }

    static String strExact(Object h, Object e, String v) {
        return (String) VS.compareAndExchange(h, e, v);
    }

    static String staticStr(Object e, Object v) {
        return (String) VSO.compareAndExchange(e, v);
    }

    // ---- rows ----

    static final Integer SEVEN = 7;
    static final Long TWO_L = 2L;
    static final Short THREE_S = (short) 3;
    static final Character CH_A = 'A';
    static final StringBuilder SB = new StringBuilder("sb");
    static final Other OTHER = new Other();

    // kernel ids
    static final String[] KN = {"objStr", "objStrAcq", "objStrRel", "gasStr", "lenStr", "pattern", "objInt",
        "numInt", "csSb", "strExact", "staticStr"};

    record Row(String name, int k, Object init, Object e, Object v, int holder) {
        // holder: 0 a Holder, 1 an Other, 2 null
    }

    static List<Row> rows() {
        List<Row> rs = new ArrayList<>();
        for (int k : new int[] {0, 1, 2}) {
            rs.add(new Row(KN[k] + " hit-wrong", k, SEVEN, SEVEN, "n", 0));
            rs.add(new Row(KN[k] + " miss-wrong", k, SEVEN, "zz", "n", 0));
            rs.add(new Row(KN[k] + " hit-ok", k, "s", "s", "n", 0));
            rs.add(new Row(KN[k] + " miss-ok", k, "s", "zz", "n", 0));
            rs.add(new Row(KN[k] + " null-witness", k, null, null, "n", 0));
            rs.add(new Row(KN[k] + " store-wrong", k, "s", "s", SEVEN, 0));
        }
        rs.add(new Row("objStr other-holder", 0, "s", "s", "n", 1));
        rs.add(new Row("objStr null-holder", 0, "s", "s", "n", 2));
        rs.add(new Row("gasStr wrong", 3, SEVEN, null, "n", 0));
        rs.add(new Row("gasStr ok", 3, "s", null, "n", 0));
        rs.add(new Row("gasStr store-wrong", 3, "s", null, SEVEN, 0));
        rs.add(new Row("lenStr hit-wrong", 4, SEVEN, SEVEN, "n", 0));
        rs.add(new Row("lenStr hit-ok", 4, "abc", "abc", "n", 0));
        rs.add(new Row("lenStr null-witness", 4, null, null, "n", 0));
        rs.add(new Row("pattern hit-wrong", 5, SEVEN, SEVEN, "n", 0));
        rs.add(new Row("pattern hit-ok", 5, "s", "s", "n", 0));
        rs.add(new Row("objInt integer", 6, SEVEN, SEVEN, "n", 0));
        rs.add(new Row("objInt short", 6, THREE_S, THREE_S, "n", 0));
        rs.add(new Row("objInt char", 6, CH_A, CH_A, "n", 0));
        rs.add(new Row("objInt long", 6, TWO_L, TWO_L, "n", 0));
        rs.add(new Row("objInt null", 6, null, null, "n", 0));
        rs.add(new Row("objInt string", 6, "s", "zz", "n", 0));
        rs.add(new Row("numInt hit-wrong", 7, TWO_L, TWO_L, 5, 0));
        rs.add(new Row("numInt miss-wrong", 7, TWO_L, 3, 5, 0));
        rs.add(new Row("numInt hit-ok", 7, SEVEN, SEVEN, 5, 0));
        rs.add(new Row("numInt expected-string", 7, SEVEN, "zz", 5, 0));
        rs.add(new Row("numInt store-string", 7, SEVEN, SEVEN, "zz", 0));
        rs.add(new Row("csSb hit-wrong", 8, "s", "s", SB, 0));
        rs.add(new Row("csSb miss-wrong", 8, "s", "zz", SB, 0));
        rs.add(new Row("csSb hit-ok", 8, SB, SB, "t", 0));
        rs.add(new Row("strExact expected-integer", 9, "s", SEVEN, "n", 0));
        rs.add(new Row("strExact hit", 9, "s", "s", "n", 0));
        rs.add(new Row("strExact other-holder", 9, "s", "s", "n", 1));
        rs.add(new Row("staticStr hit-wrong", 10, SEVEN, SEVEN, "n", 0));
        rs.add(new Row("staticStr miss-wrong", 10, SEVEN, "zz", "n", 0));
        rs.add(new Row("staticStr hit-ok", 10, "s", "s", "n", 0));
        return rs;
    }

    static String r(Object o) {
        if (o == null) {
            return "null";
        }
        if (o instanceof String) {
            return "\"" + o + "\"";
        }
        return o.getClass().getSimpleName() + ":" + o;
    }

    static Object field(Row row, Holder h) {
        switch (row.k) {
            case 7: return h.n;
            case 8: return h.cs;
            case 9: return h.s;
            case 10: return SO;
            default: return h.o;
        }
    }

    static void init(Row row, Holder h) {
        switch (row.k) {
            case 7: h.n = (Number) row.init; break;
            case 8: h.cs = (CharSequence) row.init; break;
            case 9: h.s = (String) row.init; break;
            case 10: SO = row.init; break;
            default: h.o = row.init;
        }
    }

    static Object call(Row row, Holder h) {
        Object hold = row.holder == 0 ? h : row.holder == 1 ? OTHER : null;
        switch (row.k) {
            case 0: return objStr(hold, row.e, row.v);
            case 1: return objStrAcq(hold, row.e, row.v);
            case 2: return objStrRel(hold, row.e, row.v);
            case 3: return gasStr(hold, row.v);
            case 4: return lenStr(hold, row.e, row.v);
            case 5: return pattern(hold, row.e, row.v);
            case 6: return objInt(hold, row.e, row.v);
            case 7: return numInt(hold, row.e, row.v);
            case 8: return csSb(hold, row.e, row.v);
            case 9: return strExact(hold, row.e, (String) row.v);
            default: return staticStr(row.e, row.v);
        }
    }

    static String run(Row row, boolean msg) {
        Holder h = new Holder();
        init(row, h);
        String out;
        try {
            out = "ok " + r(call(row, h));
        } catch (NullPointerException t) {
            out = "EXC java.lang.NullPointerException";
        } catch (Throwable t) {
            out = "EXC " + t.getClass().getName() + (msg ? " msg=" + t.getMessage() : "");
        }
        return out + " field=" + r(field(row, h));
    }

    static long sink;

    static void warm() {
        Holder h = new Holder();
        for (int i = 0; i < 30_000; i++) {
            String a = (i & 1) == 0 ? "p" : "q";
            String b = (i & 1) == 0 ? "q" : "p";
            h.o = a;
            sink += objStr(h, a, b).length();
            sink += objStrAcq(h, b, a).length();
            sink += objStrRel(h, a, b).length();
            sink += gasStr(h, a).length();
            sink += lenStr(h, a, b);
            sink += pattern(h, b, a).length();
            h.o = i;
            sink += objInt(h, i, i + 1);
            h.n = i;
            sink += numInt(h, i, i + 1);
            h.cs = SB;
            sink += csSb(h, SB, SB).length();
            h.s = a;
            sink += strExact(h, a, b).length();
            SO = a;
            sink += staticStr(a, b).length();
        }
    }

    public static void main(String[] args) {
        List<Row> rows = rows();
        List<String> cold = new ArrayList<>();
        for (Row row : rows) {
            String s = run(row, true);
            cold.add(s);
            System.out.println("cold " + row.name + " -> " + s);
        }
        warm();
        System.out.println("warmed " + (sink > 0));
        int same = 0;
        for (int k = 0; k < rows.size(); k++) {
            String s = run(rows.get(k), true);
            if (s.equals(cold.get(k))) {
                same++;
            } else {
                System.out.println("warm differs " + rows.get(k).name + " -> " + s);
            }
        }
        System.out.println("warm same=" + same + "/" + rows.size());
        for (int k = 0; k < rows.size(); k++) {
            Row row = rows.get(k);
            String want = run(row, false);
            String wantCold = stripMsg(cold.get(k));
            int hot = 0;
            String last = null;
            for (int i = 0; i < 20_000; i++) {
                String s = run(row, false);
                if (s.equals(wantCold)) {
                    hot++;
                } else {
                    last = s;
                }
            }
            System.out.println("hot " + row.name + " same=" + hot
                    + (hot == 20_000 ? "" : " differs: " + last)
                    + (want.equals(wantCold) ? "" : " first: " + want));
        }
    }

    static String stripMsg(String s) {
        int m = s.indexOf(" msg=");
        if (m < 0) {
            return s;
        }
        return s.substring(0, m) + s.substring(s.lastIndexOf(" field="));
    }
}
