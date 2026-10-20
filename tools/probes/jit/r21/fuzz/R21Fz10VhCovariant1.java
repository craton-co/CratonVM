// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 25, lane fuzz10: reference-array stores through
// VarHandles (and the other non-Unsafe store doors) where the STATIC array
// type is wider than the real one: an Object[] that is really a String[], a
// CharSequence[] that is a String[], a Number[] that is an Integer[], an
// Object[][] that is a String[][], and an Object[] that is an array of a
// HIDDEN class. Cold, then hot; every row prints exception type AND message
// (or the stored element / CAS answer).
//
// What JDK 25 does (VarHandleReferences.Array, read from the JDK 25 sources):
//   set           arrayType.cast(array); componentType.cast(value); then a
//                 plain aastore -> ArrayStoreException(<value class name>)
//   setVolatile / setRelease / setOpaque / getAndSet* / compareAndSet /
//   compareAndExchange*: checkIndex first, then componentType.cast(expected),
//                 then runtimeTypeCheck(value): if the handle's arrayType is
//                 NOT the array's exact class, the value is checked against
//                 the array's REAL component type and a failure is a NEW
//                 ArrayStoreException() with NO message
//   a value that is not a handle-component instance fails in asType, before
//   the body: ClassCastException "Cannot cast X to Y" (Class.cast)
// Other doors: MethodHandles.arrayElementSetter(Object[].class) is a plain
// aastore; java.lang.reflect.Array.set is IllegalArgumentException "array
// element type mismatch"; Arrays.fill is aastore; System.arraycopy is
// "arraycopy: element type mismatch: ..." / "arraycopy: type mismatch: ..."
// and keeps the elements copied before the bad one.
//
// Risk targeted: the opt-in vh-const reference array store
// (CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE, `vh_array_store_refusal`) folding
// the handle's STATIC component type instead of the array's real one, the
// compiled store check for hidden-class components, and message texts.
//
// Run:
//   javac -d out R21Fz10VhCovariant1.java
//   java -XX:-OmitStackTraceInFastThrow -Xmx256m -cp out R21Fz10VhCovariant1   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz10VhCovariant1                              # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz10VhCovariant1
//   cratonvm --nojit -Xmx256m -cp out R21Fz10VhCovariant1
//   (extra arm: CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE=1)
// -XX:-OmitStackTraceInFastThrow is REQUIRED on HotSpot: hot aastore failures
// otherwise become the preallocated message-less ArrayStoreException.
//
// HotSpot 25 expectation (not yet run). ASE = java.lang.ArrayStoreException,
// "ASE()" = an ArrayStoreException with no message; H = the hidden class's
// name with its address suffix replaced by #H (R21Fz10VhCovariant1$Hc/#H).
// First line: "hidden shape=true suffixHex=true". Each row line is
// "<row> = <answer> elem=<element 0 afterwards>"; a throwing row leaves
// elem=null, a store row prints stored / old=null / cas=true / cae=null and
// its value as elem:
//   O/covS set Integer            = ASE: java.lang.Integer
//   O/covS setVolatile Integer    = ASE()      (same for setRelease, setOpaque,
//                                   getAndSet, getAndSetAcquire, CAS(null->v),
//                                   CAE(null->v), CAEAcquire, CAERelease)
//   O/covS set String / every mode String = stored s (CAS true, CAE null, getAndSet null)
//   O/covS set null / every mode null    = stored null
//   O/covS CAS expected Integer          = false (no store; the value is checked)
//   O/covS CAS expected Integer, value Integer = ASE()
//   O/covS setVolatile index 5 Integer   = java.lang.ArrayIndexOutOfBoundsException:
//                                          Index 5 out of bounds for length 2
//   O/covS set index 5 Integer           = the same AIOOBE (aastore checks the index first)
//   O/obj  every mode Integer            = stored 42
//   CS/covCS set StringBuilder           = ASE: java.lang.StringBuilder
//   CS/covCS setVolatile StringBuilder   = ASE()   (every other mode the same)
//   CS/covCS set Integer                 = java.lang.ClassCastException: Cannot cast
//                                          java.lang.Integer to java.lang.CharSequence (every mode)
//   CS/cs  every mode StringBuilder      = stored sb
//   N/covN set Double                    = ASE: java.lang.Double ; setVolatile = ASE()
//   OO/covAA set Integer[]               = ASE: [Ljava.lang.Integer; ; setVolatile = ASE()
//   OO/covAA set Object[]                = ASE: [Ljava.lang.Object; ; setVolatile = ASE()
//   OO/covAA set String[]                = stored String[]
//   O/covH set String                    = ASE: java.lang.String ; setVolatile = ASE()
//   O/covS set hidden                    = ASE: H ; setVolatile = ASE()
//   HV/covH set String                   = java.lang.ClassCastException: Cannot cast java.lang.String to H
//   HV/covH set hidden                   = stored H
//   HHV/covHH set String[]               = java.lang.ClassCastException: Cannot cast
//                                          [Ljava.lang.String; to [LH;
//   mhSetter covS Integer                = ASE: java.lang.Integer
//   reflect covS Integer                 = java.lang.IllegalArgumentException: array element type mismatch
//   reflect covH String                  = the same
//   fill covS Integer                    = ASE: java.lang.Integer
//   arraycopy Object[]{a,b,42,d} -> covS = ASE: arraycopy: element type mismatch: can not cast one of
//                                          the elements of java.lang.Object[] to the type of the
//                                          destination array, java.lang.String ; dest=[a, b, null, null]
//   arraycopy Integer[] -> covS          = ASE: arraycopy: type mismatch: can not copy
//                                          java.lang.Integer[] into java.lang.String[] ; dest=[null x4]
//   arraycopy String[] -> covH           = ASE: arraycopy: type mismatch: can not copy
//                                          java.lang.String[] into H[]
//   arraycopy Object[]{h,b,h,h} -> covH  = ASE: arraycopy: element type mismatch: can not cast one of
//                                          the elements of java.lang.Object[] to the type of the
//                                          destination array, H ; dest=[H, null, null, null]
//   last line: "hot rows=<n> allSame=<n>" (a row whose hot answer ever differs
//   from its cold one prints its own "hot <row> same=<k> firstDiff=..." first)
// The two arraycopy texts and the hidden arraycopy text are the most likely
// header fixes after the first HotSpot run.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public class R21Fz10VhCovariant1 {
    static final int HOT = 20_000;

    static final VarHandle O = MethodHandles.arrayElementVarHandle(Object[].class);
    static final VarHandle CS = MethodHandles.arrayElementVarHandle(CharSequence[].class);
    static final VarHandle N = MethodHandles.arrayElementVarHandle(Number[].class);
    static final VarHandle OO = MethodHandles.arrayElementVarHandle(Object[][].class);
    static final MethodHandle MH_SET = MethodHandles.arrayElementSetter(Object[].class);

    static VarHandle HV;
    static VarHandle HHV;
    static Class<?> HC;
    static Object HOBJ;
    static String HSUFFIX = "#unset#";

    // Each mode is its own static method: one call site per mode, inlined hot.
    static Object oSet(Object[] a, int i, Object v) {
        O.set(a, i, v);
        return "stored";
    }

    static Object oSetVolatile(Object[] a, int i, Object v) {
        O.setVolatile(a, i, v);
        return "stored";
    }

    static Object oSetRelease(Object[] a, int i, Object v) {
        O.setRelease(a, i, v);
        return "stored";
    }

    static Object oSetOpaque(Object[] a, int i, Object v) {
        O.setOpaque(a, i, v);
        return "stored";
    }

    static Object oGetAndSet(Object[] a, int i, Object v) {
        return "old=" + show(O.getAndSet(a, i, v));
    }

    static Object oGetAndSetAcquire(Object[] a, int i, Object v) {
        return "old=" + show(O.getAndSetAcquire(a, i, v));
    }

    static Object oCas(Object[] a, int i, Object e, Object v) {
        return "cas=" + O.compareAndSet(a, i, e, v);
    }

    static Object oCae(Object[] a, int i, Object e, Object v) {
        return "cae=" + show(O.compareAndExchange(a, i, e, v));
    }

    static Object oCaeAcquire(Object[] a, int i, Object e, Object v) {
        return "cae=" + show(O.compareAndExchangeAcquire(a, i, e, v));
    }

    static Object oCaeRelease(Object[] a, int i, Object e, Object v) {
        return "cae=" + show(O.compareAndExchangeRelease(a, i, e, v));
    }

    static Object csSet(CharSequence[] a, Object v) {
        CS.set(a, 0, v);
        return "stored";
    }

    static Object csSetVolatile(CharSequence[] a, Object v) {
        CS.setVolatile(a, 0, v);
        return "stored";
    }

    static Object csGetAndSet(CharSequence[] a, Object v) {
        return "old=" + show(CS.getAndSet(a, 0, v));
    }

    static Object csCas(CharSequence[] a, Object v) {
        return "cas=" + CS.compareAndSet(a, 0, (CharSequence) null, v);
    }

    static Object csSetSb(CharSequence[] a, StringBuilder v) {
        CS.set(a, 0, v);
        return "stored";
    }

    static Object csSetVolatileSb(CharSequence[] a, StringBuilder v) {
        CS.setVolatile(a, 0, v);
        return "stored";
    }

    static Object csCasSb(CharSequence[] a, StringBuilder v) {
        return "cas=" + CS.compareAndSet(a, 0, (CharSequence) null, v);
    }

    static Object nSet(Number[] a, Number v) {
        N.set(a, 0, v);
        return "stored";
    }

    static Object nSetVolatile(Number[] a, Number v) {
        N.setVolatile(a, 0, v);
        return "stored";
    }

    static Object ooSet(Object[][] a, Object[] v) {
        OO.set(a, 0, v);
        return "stored";
    }

    static Object ooSetVolatile(Object[][] a, Object[] v) {
        OO.setVolatile(a, 0, v);
        return "stored";
    }

    static Object hvSet(Object[] a, Object v) {
        HV.set(a, 0, v);
        return "stored";
    }

    static Object hhvSet(Object[][] a, Object v) {
        HHV.set(a, 0, v);
        return "stored";
    }

    static Object mhSet(Object[] a, Object v) throws Throwable {
        MH_SET.invokeExact(a, 0, v);
        return "stored";
    }

    static Object reflectSet(Object a, Object v) {
        Array.set(a, 0, v);
        return "stored";
    }

    static Object fill(Object[] a, Object v) {
        Arrays.fill(a, v);
        return "stored";
    }

    static Object copy(Object src, Object[] dst) {
        Arrays.fill(dst, null);
        try {
            System.arraycopy(src, 0, dst, 0, Array.getLength(src));
            return "copied dest=" + shows(dst);
        } catch (ArrayStoreException e) {
            return "java.lang.ArrayStoreException: " + e.getMessage() + " ; dest=" + shows(dst);
        }
    }

    static String show(Object o) {
        if (o == null) {
            return "null";
        }
        if (o instanceof String || o instanceof Integer || o instanceof StringBuilder || o instanceof Double) {
            return String.valueOf(o);
        }
        if (o.getClass().isArray()) {
            return o.getClass().getSimpleName();
        }
        return o.getClass().getName();
    }

    static String shows(Object[] a) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < a.length; i++) {
            sb.append(i == 0 ? "" : ", ").append(show(a[i]));
        }
        return sb.append(']').toString();
    }

    static String mask(String s) {
        return s.replace(HSUFFIX, "#H");
    }

    interface Call {
        Object call() throws Throwable;
    }

    static String safe(Call c) {
        try {
            Object r = c.call();
            return mask(r instanceof String ? (String) r : show(r));
        } catch (Throwable t) {
            String m = t.getMessage();
            return mask(t.getClass().getName() + (m == null ? "()" : ": " + m)
                    + (t.getCause() == null ? "" : " cause=" + t.getCause().getClass().getName()));
        }
    }

    static final class Row {
        final String name;
        final Call call;
        final Runnable reset;
        final Call readBack;

        Row(String name, Runnable reset, Call call, Call readBack) {
            this.name = name;
            this.call = call;
            this.reset = reset;
            this.readBack = readBack;
        }

        String once() {
            reset.run();
            String r = safe(call);
            return r + " elem=" + safe(readBack);
        }
    }

    static final List<Row> ROWS = new ArrayList<>();

    static void row(String name, Object[] arr, Call call) {
        ROWS.add(new Row(name, () -> Arrays.fill(arr, null), call, () -> show(arr[0])));
    }

    static void hidden() throws Throwable {
        byte[] b = ClassFile.of().build(ClassDesc.of("R21Fz10VhCovariant1$Hc"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
        });
        MethodHandles.Lookup hl = MethodHandles.lookup().defineHiddenClass(b, true);
        HC = hl.lookupClass();
        String n = HC.getName();
        HSUFFIX = n.substring(n.lastIndexOf('/') + 1);
        HOBJ = hl.findConstructor(HC, MethodType.methodType(void.class)).invoke();
        HV = MethodHandles.arrayElementVarHandle(HC.arrayType());
        HHV = MethodHandles.arrayElementVarHandle(HC.arrayType().arrayType());
    }

    public static void main(String[] args) throws Throwable {
        hidden();
        final Object[] covS = new String[2];
        final Object[] obj = new Object[2];
        final CharSequence[] covCS = new String[2];
        final CharSequence[] cs = new CharSequence[2];
        final Number[] covN = new Integer[2];
        final Object[][] covAA = new String[2][];
        final Object[] covH = (Object[]) Array.newInstance(HC, 2);
        final Object[][] covHH = (Object[][]) Array.newInstance(HC.arrayType(), 2);
        final Integer i42 = 42;
        final StringBuilder sb = new StringBuilder("sb");
        System.out.println("hidden shape=" + HC.getName().equals("R21Fz10VhCovariant1$Hc/" + HSUFFIX)
                + " suffixHex=" + HSUFFIX.matches("0x[0-9a-fA-F]+"));

        Object[][] vals = {{"Integer", i42}, {"String", "s"}, {"null", null}};
        for (Object[] v : vals) {
            String vn = (String) v[0];
            Object val = v[1];
            row("O/covS set " + vn, covS, () -> oSet(covS, 0, val));
            row("O/covS setVolatile " + vn, covS, () -> oSetVolatile(covS, 0, val));
            row("O/covS setRelease " + vn, covS, () -> oSetRelease(covS, 0, val));
            row("O/covS setOpaque " + vn, covS, () -> oSetOpaque(covS, 0, val));
            row("O/covS getAndSet " + vn, covS, () -> oGetAndSet(covS, 0, val));
            row("O/covS getAndSetAcquire " + vn, covS, () -> oGetAndSetAcquire(covS, 0, val));
            row("O/covS CAS " + vn, covS, () -> oCas(covS, 0, null, val));
            row("O/covS CAE " + vn, covS, () -> oCae(covS, 0, null, val));
            row("O/covS CAEAcquire " + vn, covS, () -> oCaeAcquire(covS, 0, null, val));
            row("O/covS CAERelease " + vn, covS, () -> oCaeRelease(covS, 0, null, val));
            row("O/obj set " + vn, obj, () -> oSet(obj, 0, val));
            row("O/obj setVolatile " + vn, obj, () -> oSetVolatile(obj, 0, val));
            row("O/obj CAE " + vn, obj, () -> oCae(obj, 0, null, val));
        }
        row("O/covS CAS expected Integer", covS, () -> oCas(covS, 0, i42, "s"));
        row("O/covS CAS expected Integer value Integer", covS, () -> oCas(covS, 0, i42, i42));
        row("O/covS setVolatile index 5 Integer", covS, () -> oSetVolatile(covS, 5, i42));
        row("O/covS set index 5 Integer", covS, () -> oSet(covS, 5, i42));
        row("O/covS CAE index -1 Integer", covS, () -> oCae(covS, -1, null, i42));

        row("CS/covCS set StringBuilder", covCS, () -> csSetSb(covCS, sb));
        row("CS/covCS setVolatile StringBuilder", covCS, () -> csSetVolatileSb(covCS, sb));
        row("CS/covCS CAS StringBuilder", covCS, () -> csCasSb(covCS, sb));
        row("CS/covCS set Integer", covCS, () -> csSet(covCS, i42));
        row("CS/covCS setVolatile Integer", covCS, () -> csSetVolatile(covCS, i42));
        row("CS/covCS getAndSet Integer", covCS, () -> csGetAndSet(covCS, i42));
        row("CS/covCS CAS Integer", covCS, () -> csCas(covCS, i42));
        row("CS/covCS set String", covCS, () -> csSet(covCS, "s"));
        row("CS/cs set StringBuilder", cs, () -> csSetSb(cs, sb));
        row("CS/cs setVolatile StringBuilder", cs, () -> csSetVolatileSb(cs, sb));
        row("CS/cs CAS StringBuilder", cs, () -> csCasSb(cs, sb));

        row("N/covN set Double", covN, () -> nSet(covN, 4.5));
        row("N/covN setVolatile Double", covN, () -> nSetVolatile(covN, 4.5));
        row("N/covN setVolatile Integer", covN, () -> nSetVolatile(covN, i42));

        row("OO/covAA set Integer[]", covAA, () -> ooSet(covAA, new Integer[1]));
        row("OO/covAA setVolatile Integer[]", covAA, () -> ooSetVolatile(covAA, new Integer[1]));
        row("OO/covAA set Object[]", covAA, () -> ooSet(covAA, new Object[1]));
        row("OO/covAA setVolatile Object[]", covAA, () -> ooSetVolatile(covAA, new Object[1]));
        row("OO/covAA set String[]", covAA, () -> ooSet(covAA, new String[1]));
        row("OO/covAA setVolatile String[]", covAA, () -> ooSetVolatile(covAA, new String[1]));

        row("O/covH set String", covH, () -> oSet(covH, 0, "s"));
        row("O/covH setVolatile String", covH, () -> oSetVolatile(covH, 0, "s"));
        row("O/covH CAS String", covH, () -> oCas(covH, 0, null, "s"));
        row("O/covH set hidden", covH, () -> oSet(covH, 0, HOBJ));
        row("O/covS set hidden", covS, () -> oSet(covS, 0, HOBJ));
        row("O/covS setVolatile hidden", covS, () -> oSetVolatile(covS, 0, HOBJ));
        row("HV/covH set String", covH, () -> hvSet(covH, "s"));
        row("HV/covH set hidden", covH, () -> hvSet(covH, HOBJ));
        row("HHV/covHH set String[]", covHH, () -> hhvSet(covHH, new String[1]));
        row("O/covHH set String[]", covHH, () -> oSet(covHH, 0, new String[1]));

        row("mhSetter covS Integer", covS, () -> mhSet(covS, i42));
        row("mhSetter covH String", covH, () -> mhSet(covH, "s"));
        row("reflect covS Integer", covS, () -> reflectSet(covS, i42));
        row("reflect covH String", covH, () -> reflectSet(covH, "s"));
        row("reflect covS String", covS, () -> reflectSet(covS, "s"));
        row("fill covS Integer", covS, () -> fill(covS, i42));
        row("fill covH hidden", covH, () -> fill(covH, HOBJ));

        final String[] dst4 = new String[4];
        ROWS.add(new Row("arraycopy Object[] -> String[]", () -> { }, () -> copy(new Object[] {"a", "b", i42, "d"},
                dst4), () -> "-"));
        ROWS.add(new Row("arraycopy Integer[] -> String[]", () -> { }, () -> copy(new Integer[] {1, 2, 3, 4},
                dst4), () -> "-"));
        final Object[] h4 = (Object[]) Array.newInstance(HC, 4);
        ROWS.add(new Row("arraycopy String[] -> hidden[]", () -> { }, () -> copy(new String[] {"a", "b", "c", "d"},
                h4), () -> "-"));
        ROWS.add(new Row("arraycopy Object[] -> hidden[]", () -> { }, () -> copy(new Object[] {HOBJ, "b", HOBJ, HOBJ},
                h4), () -> "-"));

        String[] cold = new String[ROWS.size()];
        for (int r = 0; r < ROWS.size(); r++) {
            cold[r] = ROWS.get(r).once();
            System.out.println(ROWS.get(r).name + " = " + cold[r]);
        }
        // Hot: 20000 rounds, every row once per round (interleaved).
        int[] same = new int[ROWS.size()];
        String[] firstDiff = new String[ROWS.size()];
        for (int round = 0; round < HOT; round++) {
            for (int r = 0; r < ROWS.size(); r++) {
                String got = ROWS.get(r).once();
                if (got.equals(cold[r])) {
                    same[r]++;
                } else if (firstDiff[r] == null) {
                    firstDiff[r] = got + " at round " + round;
                }
            }
        }
        int allSame = 0;
        for (int r = 0; r < ROWS.size(); r++) {
            if (same[r] == HOT) {
                allSame++;
            } else {
                System.out.println("hot " + ROWS.get(r).name + " same=" + same[r] + " firstDiff=" + firstDiff[r]);
            }
        }
        System.out.println("hot rows=" + ROWS.size() + " allSame=" + allSame);
    }
}
