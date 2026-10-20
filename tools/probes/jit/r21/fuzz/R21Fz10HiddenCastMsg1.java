// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 25, lane fuzz10: the class-naming texts (getName family,
// Class.cast, checkcast, instanceof, aastore) for hidden classes, arrays of
// hidden classes, and ORDINARY classes whose names only look hidden.
//
// Classes:
//   hidden (app loader, Lookup.defineHiddenClass, built with java.lang.classfile;
//   each has self(Object) = checkcast to itself, is(Object) = instanceof itself):
//     A  R21Fz10HiddenCastMsg1$Hd
//     B  R21Fz10HiddenCastMsg1$Q+0x7     a '+' in the name: HotSpot mangles a
//                                        hidden name with '+' and turns only the
//                                        LAST '+' into '/', so B's name is
//                                        R21Fz10HiddenCastMsg1$Q+0x7/0x<addr>
//     C  0x1F                            a default-package name that is itself
//                                        address-shaped: 0x1F/0x<addr>
//   ordinary (loader named "fz10c"):
//     fz10c.Plus+0x1F                    '+0x..' tail, NOT hidden: keeps its '+'
//     fz10c.Seg.0x2A                     internal fz10c/Seg/0x2A: a '/0x..' tail
//     fz10c.Caster                       toSeg / toPlus: checkcast in that loader
// Each hidden class's address suffix (the text after the last '/' of its OWN
// getName()) is replaced by #A, #B or #C wherever it appears; nothing else is
// masked except loader hashes (" @<hex>" -> " @*"). So a text that names the
// wrong hidden class, drops the suffix, or rewrites an ordinary name shows.
// Every row runs cold (printed) and then 20000 times hot, compared with cold.
//
// Risk targeted: wave 24's renderers that name an array by its BOTTOM class's
// hidden flag (unknown ids keep the shape rule), the shape rule itself on
// ordinary '/0x' and '+0x' names, and the compiled checkcast / aastore throw
// paths that build the same texts.
//
// Run:
//   javac -d out R21Fz10HiddenCastMsg1.java
//   java -XX:-OmitStackTraceInFastThrow -Xmx256m -cp out R21Fz10HiddenCastMsg1   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz10HiddenCastMsg1                              # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz10HiddenCastMsg1
//   cratonvm --nojit -Xmx256m -cp out R21Fz10HiddenCastMsg1
// (-XX:-OmitStackTraceInFastThrow: hot checkcast / aastore failures keep their
// messages on C2.)
//
// HotSpot 25 expectation (by reading Klass::external_name /
// convert_hidden_name_to_java, ClassFileParser::mangle_hidden_class_name,
// SharedRuntime::generate_class_cast_message, Klass::class_in_module_of_loader
// / joint_in_module_of_loader, InterpreterRuntime::throw_ArrayStoreException,
// and Class.getSimpleName0 / getTypeName / getCanonicalName0 /
// descriptorString / cast in JDK 25; not yet run). Write N(A) for
// R21Fz10HiddenCastMsg1$Hd/#A, N(B) for R21Fz10HiddenCastMsg1$Q+0x7/#B, N(C)
// for 0x1F/#C, and APP for "is in unnamed module of loader 'app'":
//   A shape=true  B shape=true  C shape=true   (name = own bytes' name + "/0x" + hex)
//   A name=N(A) arr=[LN(A); arr2=[[LN(A);
//   A simple=N(A) arrSimple=N(A)[] type=N(A) arrType=N(A)[][] canon=null arrCanon=null
//   A desc=LR21Fz10HiddenCastMsg1$Hd.#A; arrDesc=[LR21Fz10HiddenCastMsg1$Hd.#A; str=class N(A)
//   (B and C the same, with C's desc L0x1F.#C; and B's simple N(B))
//   A cast=java.lang.ClassCastException: Cannot cast java.lang.String to N(A)
//   A arrCast=java.lang.ClassCastException: Cannot cast java.lang.String to [LN(A);
//   A arr2Cast=java.lang.ClassCastException: Cannot cast [LN(A); to [[LN(A);
//   A strCast=java.lang.ClassCastException: Cannot cast N(A) to java.lang.String
//   A strArrCast=java.lang.ClassCastException: Cannot cast [LN(A); to [Ljava.lang.String;
//   A cc String=java.lang.ClassCastException: class N(A) cannot be cast to class java.lang.String
//        (N(A) APP; java.lang.String is in module java.base of loader 'bootstrap')
//   A cc String[]=... class [LN(A); cannot be cast to class [Ljava.lang.String; ([LN(A); APP;
//        [Ljava.lang.String; is in module java.base of loader 'bootstrap')
//   A cc Runnable=... class N(A) cannot be cast to class java.lang.Runnable (N(A) APP;
//        java.lang.Runnable is in module java.base of loader 'bootstrap')
//   A cc main=... class N(A) cannot be cast to class R21Fz10HiddenCastMsg1 (N(A) and
//        R21Fz10HiddenCastMsg1 are in unnamed module of loader 'app')
//   A self String=... class java.lang.String cannot be cast to class N(A)
//        (java.lang.String is in module java.base of loader 'bootstrap'; N(A) APP)
//   A self main=... class R21Fz10HiddenCastMsg1 cannot be cast to class N(A)
//        (R21Fz10HiddenCastMsg1 and N(A) are in unnamed module of loader 'app')
//   A self other=... class N(B) cannot be cast to class N(A) (N(B) and N(A) are in
//        unnamed module of loader 'app')           (B's row: other = C, C's: other = A)
//   A self own=same   A is s=0 own=1 other=0 arrIsObjArr=true arrIsInstance=true
//   A aastore obj=java.lang.ArrayStoreException: N(A)
//   A aastore arr=java.lang.ArrayStoreException: [LN(A);
//   A aastore into=java.lang.ArrayStoreException: java.lang.String
//   A aastore cross=java.lang.ArrayStoreException: N(A)   (A into B[]; B into C[]; C into A[])
//   Plus name=fz10c.Plus+0x1F arr=[Lfz10c.Plus+0x1F; arr2=[[Lfz10c.Plus+0x1F;
//   Plus simple=Plus+0x1F arrSimple=Plus+0x1F[] type=fz10c.Plus+0x1F arrType=fz10c.Plus+0x1F[][]
//        canon=fz10c.Plus+0x1F arrCanon=fz10c.Plus+0x1F[]
//   Plus desc=Lfz10c/Plus+0x1F; arrDesc=[Lfz10c/Plus+0x1F; str=class fz10c.Plus+0x1F
//   Seg  name=fz10c.Seg.0x2A ... simple=0x2A arrSimple=0x2A[] desc=Lfz10c/Seg/0x2A;
//   Plus cc String=... class fz10c.Plus+0x1F cannot be cast to class java.lang.String
//        (fz10c.Plus+0x1F is in unnamed module of loader 'fz10c' @*; java.lang.String is in
//        module java.base of loader 'bootstrap')
//   cross Plus toSeg=... class fz10c.Plus+0x1F cannot be cast to class fz10c.Seg.0x2A
//        (fz10c.Plus+0x1F and fz10c.Seg.0x2A are in unnamed module of loader 'fz10c' @*)
//   cross Seg toPlus=... class fz10c.Seg.0x2A cannot be cast to class fz10c.Plus+0x1F (... 'fz10c' @*)
//   cross hidden toPlus=... class N(A) cannot be cast to class fz10c.Plus+0x1F (N(A) APP;
//        fz10c.Plus+0x1F is in unnamed module of loader 'fz10c' @*)
//   Plus aastore obj=java.lang.ArrayStoreException: fz10c.Plus+0x1F
//   Seg  aastore arr=java.lang.ArrayStoreException: [Lfz10c.Seg.0x2A;
//   Plus cast=java.lang.ClassCastException: Cannot cast java.lang.String to fz10c.Plus+0x1F
//   Seg  arr2Cast=java.lang.ClassCastException: Cannot cast [Lfz10c.Seg.0x2A; to [[Lfz10c.Seg.0x2A;
//   cross Plus toSeg / Seg toPlus / hidden toPlus as above; cross hiddenArr toSeg=... class [LN(A); cannot
//        be cast to class fz10c.Seg.0x2A ([LN(A); APP; fz10c.Seg.0x2A is in unnamed module of loader 'fz10c' @*)
//   cross Plus self A=... class fz10c.Plus+0x1F cannot be cast to class N(A) (fz10c.Plus+0x1F is in unnamed
//        module of loader 'fz10c' @*; N(A) APP)
//   cross Seg into A[]=java.lang.ArrayStoreException: fz10c.Seg.0x2A
//   cross A[] into Plus[][]=java.lang.ArrayStoreException: [LN(A);
//   hot <group> rows=<n> same=20000 (one line per group, every row equal to its cold text)
// ("..." = java.lang.ClassCastException: ; the probe prints each text on one line.)

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.regex.Pattern;

public class R21Fz10HiddenCastMsg1 {
    static final int HOT = 20_000;
    static final MethodTypeDesc MTD_O_O = MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object);
    static final MethodTypeDesc MTD_I_O = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object);

    interface Call {
        Object call() throws Throwable;
    }

    // suffix -> label, longest suffix first
    static final List<String[]> SUFFIXES = new ArrayList<>();

    static String mask(String s) {
        if (s == null) {
            return "null";
        }
        for (String[] p : SUFFIXES) {
            s = s.replace(p[0], p[1]);
        }
        return s.replaceAll(" @[0-9a-fA-F]+", " @*");
    }

    static String safe(Call c) {
        try {
            Object r = c.call();
            return r == null ? "null" : r instanceof String ? (String) r : "ok " + r.getClass().getName();
        } catch (Throwable t) {
            String m = t.getMessage();
            return t.getClass().getName() + (m == null ? "" : ": " + m);
        }
    }

    /** A public class with a public no-arg ctor; `self` adds self(Object) and is(Object). */
    static byte[] cls(ClassDesc me, boolean self) {
        return ClassFile.of().build(me, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
            if (self) {
                cb.withMethodBody("self", MTD_O_O, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        code -> code.aload(0).checkcast(me).areturn());
                cb.withMethodBody("is", MTD_I_O, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        code -> code.aload(0).instanceOf(me).ireturn());
            }
        });
    }

    static final ClassDesc PLUS = ClassDesc.of("fz10c.Plus+0x1F");
    static final ClassDesc SEG = ClassDesc.of("fz10c.Seg.0x2A");
    static final ClassDesc CASTER = ClassDesc.of("fz10c.Caster");

    static byte[] caster() {
        return ClassFile.of().build(CASTER, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("toSeg", MTD_O_O, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).checkcast(SEG).areturn());
            cb.withMethodBody("toPlus", MTD_O_O, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).checkcast(PLUS).areturn());
        });
    }

    static final class L extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        L() {
            super("fz10c", R21Fz10HiddenCastMsg1.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    // ---- the casting sites (main class) ----
    static Object ccString(Object o) {
        return (String) o;
    }

    static Object ccStringArr(Object o) {
        return (String[]) o;
    }

    static Object ccRunnable(Object o) {
        return (Runnable) o;
    }

    static Object ccMain(Object o) {
        return (R21Fz10HiddenCastMsg1) o;
    }

    static Object aastore(Object[] into, Object v) {
        into[0] = v;
        return "stored";
    }

    static final class K {
        final String label;
        final Class<?> c;
        final Class<?> arr;
        final Class<?> arr2;
        final Object obj;
        final Object arrObj;
        final MethodHandle self;
        final MethodHandle is;

        K(String label, Class<?> c, MethodHandle self, MethodHandle is) throws Throwable {
            this.label = label;
            this.c = c;
            this.arr = c.arrayType();
            this.arr2 = arr.arrayType();
            this.obj = construct(c);
            this.arrObj = Array.newInstance(c, 1);
            this.self = self;
            this.is = is;
        }
    }

    static final Map<Class<?>, MethodHandles.Lookup> LOOKUPS = new HashMap<>();

    static Object construct(Class<?> c) throws Throwable {
        MethodHandles.Lookup l = LOOKUPS.get(c);
        if (l == null) {
            return c.getDeclaredConstructor().newInstance();
        }
        return l.findConstructor(c, MethodType.methodType(void.class)).invoke();
    }

    static K hidden(String label, String binaryName) throws Throwable {
        MethodHandles.Lookup hl = MethodHandles.lookup().defineHiddenClass(cls(ClassDesc.of(binaryName), true),
                false);
        Class<?> c = hl.lookupClass();
        LOOKUPS.put(c, hl);
        String n = c.getName();
        String suffix = n.substring(n.lastIndexOf('/') + 1);
        SUFFIXES.add(new String[] {suffix, "#" + label});
        SUFFIXES.sort((x, y) -> y[0].length() - x[0].length());
        System.out.println(label + " shape=" + Pattern.matches(Pattern.quote(binaryName) + "/0x[0-9a-fA-F]+", n));
        MethodHandle self = hl.findStatic(c, "self", MethodType.methodType(Object.class, Object.class));
        MethodHandle is = hl.findStatic(c, "is", MethodType.methodType(int.class, Object.class));
        return new K(label, c, self, is);
    }

    static Map<String, Call> nameRows(K k) {
        Map<String, Call> m = new LinkedHashMap<>();
        m.put("name", k.c::getName);
        m.put("arr", k.arr::getName);
        m.put("arr2", k.arr2::getName);
        m.put("simple", k.c::getSimpleName);
        m.put("arrSimple", k.arr::getSimpleName);
        m.put("type", k.c::getTypeName);
        m.put("arrType", k.arr2::getTypeName);
        m.put("canon", k.c::getCanonicalName);
        m.put("arrCanon", k.arr::getCanonicalName);
        m.put("desc", k.c::descriptorString);
        m.put("arrDesc", k.arr::descriptorString);
        m.put("str", k.c::toString);
        return m;
    }

    static Map<String, Call> castRows(K k, K other) {
        Map<String, Call> m = new LinkedHashMap<>();
        m.put("cast", () -> k.c.cast("s"));
        m.put("arrCast", () -> k.arr.cast("s"));
        m.put("arr2Cast", () -> k.arr2.cast(k.arrObj));
        m.put("strCast", () -> String.class.cast(k.obj));
        m.put("strArrCast", () -> String[].class.cast(k.arrObj));
        m.put("cc String", () -> ccString(k.obj));
        m.put("cc String[]", () -> ccStringArr(k.arrObj));
        m.put("cc Runnable", () -> ccRunnable(k.obj));
        m.put("cc main", () -> ccMain(k.obj));
        if (k.self != null) {
            m.put("self String", () -> k.self.invoke((Object) "s"));
            m.put("self main", () -> k.self.invoke((Object) new R21Fz10HiddenCastMsg1()));
            m.put("self other", () -> k.self.invoke(other.obj));
            m.put("self own", () -> k.self.invoke(k.obj) == k.obj ? "same" : "different");
            m.put("is", () -> "s=" + (int) k.is.invoke((Object) "s") + " own=" + (int) k.is.invoke(k.obj)
                    + " other=" + (int) k.is.invoke(other.obj) + " arrIsObjArr=" + (k.arrObj instanceof Object[])
                    + " arrIsInstance=" + k.arr.isInstance(k.arrObj));
        }
        m.put("aastore obj", () -> aastore(new String[1], k.obj));
        m.put("aastore arr", () -> aastore(new String[1][], k.arrObj));
        m.put("aastore into", () -> aastore((Object[]) Array.newInstance(k.c, 1), "s"));
        m.put("aastore cross", () -> aastore((Object[]) Array.newInstance(other.c, 1), k.obj));
        return m;
    }

    static int groups;

    static void group(String label, Map<String, Call> rows) {
        Map<String, String> cold = new LinkedHashMap<>();
        for (Map.Entry<String, Call> e : rows.entrySet()) {
            String r = mask(safe(e.getValue()));
            cold.put(e.getKey(), r);
            System.out.println(label + " " + e.getKey() + "=" + r);
        }
        int ok = 0;
        String firstDiff = null;
        for (int i = 0; i < HOT; i++) {
            boolean all = true;
            for (Map.Entry<String, Call> e : rows.entrySet()) {
                String r = mask(safe(e.getValue()));
                if (!r.equals(cold.get(e.getKey()))) {
                    all = false;
                    if (firstDiff == null) {
                        firstDiff = e.getKey() + "=" + r + " at " + i;
                    }
                }
            }
            if (all) {
                ok++;
            }
        }
        groups++;
        System.out.println("hot " + label + " rows=" + rows.size() + " same=" + ok
                + (firstDiff == null ? "" : " firstDiff=" + firstDiff));
    }

    public static void main(String[] args) throws Throwable {
        K a = hidden("A", "R21Fz10HiddenCastMsg1$Hd");
        K b = hidden("B", "R21Fz10HiddenCastMsg1$Q+0x7");
        K c = hidden("C", "0x1F");

        L l = new L();
        l.bytes.put("fz10c.Plus+0x1F", cls(PLUS, false));
        l.bytes.put("fz10c.Seg.0x2A", cls(SEG, false));
        l.bytes.put("fz10c.Caster", caster());
        Class<?> plusC = Class.forName("fz10c.Plus+0x1F", true, l);
        Class<?> segC = Class.forName("fz10c.Seg.0x2A", true, l);
        Class<?> casterC = Class.forName("fz10c.Caster", true, l);
        K plus = new K("Plus", plusC, null, null);
        K seg = new K("Seg", segC, null, null);
        MethodHandle toSeg = MethodHandles.lookup().findStatic(casterC, "toSeg",
                MethodType.methodType(Object.class, Object.class));
        MethodHandle toPlus = MethodHandles.lookup().findStatic(casterC, "toPlus",
                MethodType.methodType(Object.class, Object.class));

        for (K k : new K[] {a, b, c, plus, seg}) {
            group(k.label, nameRows(k));
        }
        group("A", castRows(a, b));
        group("B", castRows(b, c));
        group("C", castRows(c, a));
        group("Plus", castRows(plus, seg));
        group("Seg", castRows(seg, plus));

        Map<String, Call> cross = new LinkedHashMap<>();
        cross.put("Plus toSeg", () -> toSeg.invoke(plus.obj));
        cross.put("Seg toPlus", () -> toPlus.invoke(seg.obj));
        cross.put("hidden toPlus", () -> toPlus.invoke(a.obj));
        cross.put("hiddenArr toSeg", () -> toSeg.invoke(a.arrObj));
        cross.put("Plus self A", () -> a.self.invoke(plus.obj));
        cross.put("Seg into A[]", () -> aastore((Object[]) Array.newInstance(a.c, 1), seg.obj));
        cross.put("A[] into Plus[][]", () -> aastore((Object[]) Array.newInstance(plus.arr, 1), a.arrObj));
        group("cross", cross);
        System.out.println("groups=" + groups);
    }
}
