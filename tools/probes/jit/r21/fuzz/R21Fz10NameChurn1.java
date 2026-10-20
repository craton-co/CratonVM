// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 25, lane fuzz10: Class.getName / getSimpleName /
// getTypeName asked hot on class, array, primitive and hidden-class mirrors
// while classes are unloaded (their loaders dropped and collected) and the
// same names are defined again in new loaders. The answer for a mirror must
// always be ITS OWN name, never a stale answer left by an earlier mirror at
// the same address or an earlier class with the same id.
//
// Layout:
//   keeper loader K (live all along): fz10n.A, fz10n.G0, fz10n.Plus+0x0, and
//     one hidden class R21Fz10NameChurn1$Hn (kept strongly reachable)
//   generation g = 1..8: a new loader Lg with fz10n.A (same name as K's),
//     fz10n.G<g>, fz10n.Plus+0x<g>; three hidden R21Fz10NameChurn1$Hn and one
//     R21Fz10NameChurn1$Hn<g> (non-STRONG, unloadable); each class with its
//     [] and [][] mirrors, plus the primitive and JDK mirrors and the keeper's
//   hot loop: 20000 asks per generation, rotating over the mirrors and over
//     three call paths (direct, a MethodHandle to Class.getName, a
//     Function<Class<?>, String>); every 7th ask also checks getSimpleName and
//     getTypeName; at asks 0, 5000, 10000 and 15000 a throw-away loader defines
//     fz10n.T<g>x<i> and its array name is checked, then dropped
//   after each generation: Lg dropped, then at most 10 System.gc() until a
//     WeakReference to Lg clears (nothing about the GC is printed)
// The expected text of an ordinary class is built from the name given to
// defineClass; a hidden class's from its first getName(), which must have the
// shape <bytes name>/0x<hex>. Hidden suffixes print as /0x*.
//
// Risk targeted: wave 24's CRATONVM_CLASS_GET_NAME_MIRROR_MEMO (per-thread
// lines keyed by mirror address and epoch, noted only for strongly held
// answers) and the class-id name memo, across unload and redefinition.
//
// Run:
//   javac -d out R21Fz10NameChurn1.java
//   java -Xmx256m -cp out R21Fz10NameChurn1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz10NameChurn1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz10NameChurn1
//   cratonvm --nojit -Xmx256m -cp out R21Fz10NameChurn1
//   (extra arm: CRATONVM_CLASS_GET_NAME_MIRROR_MEMO=1)
//
// HotSpot 25 expectation (by reading Class.getSimpleName0 / getTypeName and
// Klass::external_name; not yet run):
//   cold int name=int simple=int type=int
//   cold [[J name=[[J simple=long[][] type=long[][]
//   cold void name=void simple=void type=void
//   cold [Ljava.lang.Object; name=[Ljava.lang.Object; simple=Object[] type=java.lang.Object[]
//   cold keeper A name=fz10n.A simple=A arr=[Lfz10n.A; type2=fz10n.A[][]
//   cold keeper Plus name=fz10n.Plus+0x0 simple=Plus+0x0 arrSimple=Plus+0x0[]
//   cold keeper hidden shape=true name=R21Fz10NameChurn1$Hn/0x* arr=[LR21Fz10NameChurn1$Hn/0x*;
//        simple=R21Fz10NameChurn1$Hn/0x* type2=R21Fz10NameChurn1$Hn/0x*[][]   (one line)
//   gen 1 mirrors=42 checks=25724 bad=0 shapes=true distinct=true newA=true keeperA=true
//   ... the same line for gen 2 to gen 8 ...
//   after keeper A=fz10n.A G0=fz10n.G0 hidden=R21Fz10NameChurn1$Hn/0x* stable=true
// (mirrors = (3 ordinary + 4 hidden + 3 keeper classes) x 3 mirrors + 12
// fixed = 42; checks = 20000 names + 2 x 2858 simple/type + 4 x 2 tmp = 25724.)
// A bad > 0 line also prints the first wrong answer and the expected one.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.function.Function;
import java.util.regex.Pattern;

public class R21Fz10NameChurn1 {
    static final int GENS = 8;
    static final int HOT = 20_000;
    static final String HN = "R21Fz10NameChurn1$Hn";

    static final MethodHandle GET_NAME;
    static final Function<Class<?>, String> GET_NAME_FN = Class::getName;

    static {
        try {
            GET_NAME = MethodHandles.lookup().findVirtual(Class.class, "getName", MethodType.methodType(String.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static byte[] cls(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER));
    }

    static final class L extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        L(String n, String... names) {
            super(n, R21Fz10NameChurn1.class.getClassLoader());
            for (String s : names) {
                bytes.put(s, cls(s));
            }
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

    static final class Exp {
        final Class<?> m;
        final String name;
        final String simple;
        final String type;

        Exp(Class<?> m, String name, String simple, String type) {
            this.m = m;
            this.name = name;
            this.simple = simple;
            this.type = type;
        }
    }

    static String mask(String s) {
        return s == null ? "null" : s.replaceAll("/0x[0-9a-fA-F]+", "/0x*");
    }

    /** c, c[], c[][] with the expectations derived from `name` / `simple`. */
    static void add3(List<Exp> out, Class<?> c, String name, String simple) {
        out.add(new Exp(c, name, simple, name));
        out.add(new Exp(c.arrayType(), "[L" + name + ";", simple + "[]", name + "[]"));
        out.add(new Exp(c.arrayType().arrayType(), "[[L" + name + ";", simple + "[][]", name + "[][]"));
    }

    static boolean shapes = true;

    static Class<?> hidden(String bytesName, Set<String> liveNames) throws IllegalAccessException {
        Class<?> h = MethodHandles.lookup().defineHiddenClass(cls(bytesName), false).lookupClass();
        String n = h.getName();
        if (!Pattern.matches(Pattern.quote(bytesName) + "/0x[0-9a-fA-F]+", n)) {
            shapes = false;
        }
        liveNames.add(n);
        return h;
    }

    static void fixed(List<Exp> out) {
        out.add(new Exp(int.class, "int", "int", "int"));
        out.add(new Exp(long.class, "long", "long", "long"));
        out.add(new Exp(void.class, "void", "void", "void"));
        out.add(new Exp(boolean.class, "boolean", "boolean", "boolean"));
        out.add(new Exp(int[].class, "[I", "int[]", "int[]"));
        out.add(new Exp(long[][].class, "[[J", "long[][]", "long[][]"));
        out.add(new Exp(boolean[].class, "[Z", "boolean[]", "boolean[]"));
        out.add(new Exp(char[][][].class, "[[[C", "char[][][]", "char[][][]"));
        out.add(new Exp(Object[].class, "[Ljava.lang.Object;", "Object[]", "java.lang.Object[]"));
        out.add(new Exp(String[][].class, "[[Ljava.lang.String;", "String[][]", "java.lang.String[][]"));
        out.add(new Exp(String.class, "java.lang.String", "String", "java.lang.String"));
        out.add(new Exp(Map.Entry[].class, "[Ljava.util.Map$Entry;", "Entry[]", "java.util.Map$Entry[]"));
    }

    static Class<?> keeperA;
    static Class<?> keeperG0;
    static Class<?> keeperPlus;
    static Class<?> keeperH;
    static String keeperHName;
    static L keeper;

    static String ask(Class<?> c, int path) throws Throwable {
        switch (path) {
            case 0:
                return c.getName();
            case 1:
                return (String) GET_NAME.invokeExact(c);
            default:
                return GET_NAME_FN.apply(c);
        }
    }

    static WeakReference<ClassLoader> generation(int g) throws Throwable {
        String a = "fz10n.A";
        String gn = "fz10n.G" + g;
        String plus = "fz10n.Plus+0x" + g;
        L lg = new L("gen" + g, a, gn, plus);
        List<Exp> exp = new ArrayList<>();
        Set<String> live = new HashSet<>();
        live.add(keeperHName);
        Class<?> ca = Class.forName(a, false, lg);
        add3(exp, ca, a, "A");
        add3(exp, Class.forName(gn, false, lg), gn, "G" + g);
        add3(exp, Class.forName(plus, false, lg), plus, "Plus+0x" + g);
        for (int k = 0; k < 3; k++) {
            Class<?> h = hidden(HN, live);
            String n = h.getName();
            add3(exp, h, n, n);
        }
        Class<?> hg = hidden(HN + g, live);
        add3(exp, hg, hg.getName(), hg.getName());
        fixed(exp);
        add3(exp, keeperA, "fz10n.A", "A");
        add3(exp, keeperPlus, "fz10n.Plus+0x0", "Plus+0x0");
        add3(exp, keeperH, keeperHName, keeperHName);
        boolean distinct = live.size() == 5;
        boolean newA = ca != keeperA && ca.getClassLoader() == lg;

        int checks = 0;
        int bad = 0;
        String firstBad = null;
        int size = exp.size();
        for (int i = 0; i < HOT; i++) {
            Exp e = exp.get(i % size);
            String got = ask(e.m, i % 3);
            checks++;
            if (!got.equals(e.name)) {
                bad++;
                if (firstBad == null) {
                    firstBad = "name of " + mask(e.name) + " was " + mask(got);
                }
            }
            if (i % 7 == 0) {
                String s = e.m.getSimpleName();
                String t = e.m.getTypeName();
                checks += 2;
                if (!s.equals(e.simple)) {
                    bad++;
                    if (firstBad == null) {
                        firstBad = "simple of " + mask(e.name) + " was " + mask(s);
                    }
                }
                if (!t.equals(e.type)) {
                    bad++;
                    if (firstBad == null) {
                        firstBad = "type of " + mask(e.name) + " was " + mask(t);
                    }
                }
            }
            if (i % 5000 == 0) {
                String tn = "fz10n.T" + g + "x" + i;
                L t = new L("tmp", tn);
                Class<?> tc = Class.forName(tn, false, t);
                String n1 = tc.getName();
                String n2 = tc.arrayType().getName();
                checks += 2;
                if (!n1.equals(tn)) {
                    bad++;
                    if (firstBad == null) {
                        firstBad = "tmp " + tn + " was " + mask(n1);
                    }
                }
                if (!n2.equals("[L" + tn + ";")) {
                    bad++;
                    if (firstBad == null) {
                        firstBad = "tmp [L" + tn + "; was " + mask(n2);
                    }
                }
            }
        }
        boolean keeperOk = Class.forName("fz10n.A", false, keeper) == keeperA
                && keeperA.getName().equals("fz10n.A") && keeperA.getClassLoader() == keeper;
        System.out.println("gen " + g + " mirrors=" + size + " checks=" + checks + " bad=" + bad + " shapes=" + shapes
                + " distinct=" + distinct + " newA=" + newA + " keeperA=" + keeperOk
                + (firstBad == null ? "" : " firstBad=" + firstBad));
        return new WeakReference<>(lg);
    }

    public static void main(String[] args) throws Throwable {
        keeper = new L("keeper", "fz10n.A", "fz10n.G0", "fz10n.Plus+0x0");
        keeperA = Class.forName("fz10n.A", false, keeper);
        keeperG0 = Class.forName("fz10n.G0", false, keeper);
        keeperPlus = Class.forName("fz10n.Plus+0x0", false, keeper);
        Set<String> live = new HashSet<>();
        keeperH = hidden(HN, live);
        keeperHName = keeperH.getName();

        for (Class<?> c : new Class<?>[] {int.class, long[][].class, void.class, Object[].class}) {
            System.out.println("cold " + c.getName() + " name=" + c.getName() + " simple=" + c.getSimpleName()
                    + " type=" + c.getTypeName());
        }
        System.out.println("cold keeper A name=" + keeperA.getName() + " simple=" + keeperA.getSimpleName()
                + " arr=" + keeperA.arrayType().getName() + " type2=" + keeperA.arrayType().arrayType().getTypeName());
        System.out.println("cold keeper Plus name=" + keeperPlus.getName() + " simple=" + keeperPlus.getSimpleName()
                + " arrSimple=" + keeperPlus.arrayType().getSimpleName());
        System.out.println("cold keeper hidden shape=" + shapes + " name=" + mask(keeperHName)
                + " arr=" + mask(keeperH.arrayType().getName()) + " simple=" + mask(keeperH.getSimpleName())
                + " type2=" + mask(keeperH.arrayType().arrayType().getTypeName()));

        for (int g = 1; g <= GENS; g++) {
            WeakReference<ClassLoader> w = generation(g);
            for (int k = 0; k < 10 && w.get() != null; k++) {
                System.gc();
            }
        }
        System.out.println("after keeper A=" + keeperA.getName() + " G0=" + keeperG0.getName() + " hidden="
                + mask(keeperH.getName()) + " stable=" + keeperH.getName().equals(keeperHName));
    }
}
