// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 23, lane fuzz9: getSimpleName / getCanonicalName /
// isAnonymousClass / isLocalClass / getEnclosingMethod / getEnclosingClass /
// getDeclaringClass on classes whose EnclosingMethod class cannot be loaded,
// in the APPLICATION loader (Lookup.defineClass) and in a CUSTOM loader that
// refuses that name in several ways; each fact is asked cold, then 20000
// times hot, counting answers equal to the cold one.
//
// Classes (built with java.lang.classfile):
//   app loader, default package, EM class R21Fz9EmMissing1Gone (absent):
//     A$L  row (A$L, -, "L") + EM      local
//     A$1  row (A$1, -, -)   + EM      anonymous
//   custom loader, package r21fz9e:
//     B$L  row + EM(r21fz9e.Gone)   loader throws ClassNotFoundException
//     B$1  anon row + EM(r21fz9e.Gone)
//     C$E  no InnerClasses + EM(r21fz9e.Gone)
//     D$X  row (D$X, -, "X") + EM(r21fz9e.Flaky): the loader throws
//          IllegalStateException for the first 3 asks of Flaky, then defines it
//          (Flaky lists D$X as its local class)
//     F$N  row + EM(r21fz9e.Nc): the loader throws NoClassDefFoundError("nc-custom")
//     G$W  row + EM(r21fz9e.Wrong): the loader answers r21fz9e.Other (a class
//          of another name) for Wrong
//     H$C  row (H$C, -, "C") + EM(r21fz9e.Host): control, Host lists H$C
// The NCDFE message (class name in internal form) and the cause class are
// printed; the loader's ask counts are printed at the end.
//
// Risk targeted: wave 22 item 1 (--compatible name bridges resolve the
// EnclosingMethod class again, NCDFE as HotSpot) in the custom-loader arm
// and hot; the sticky resolution error (w21 6c) for the EM constant-pool
// entry; and that a NON-LinkageError (IllegalStateException) is not recorded.
//
// Run:
//   javac -d out R21Fz9EmMissing1.java
//   java -Xmx256m -cp out R21Fz9EmMissing1                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz9EmMissing1             # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz9EmMissing1
//   cratonvm --nojit -Xmx256m -cp out R21Fz9EmMissing1
//
// HotSpot 25 expectation (by reading JDK 25 Class.java -- isLocalOrAnonymousClass
// is hasEnclosingMethodInfo, getEnclosingMethod0 -- and HotSpot's
// JVM_GetEnclosingMethodInfo (klass_at, CHECK_NULL), compute_enclosing_class,
// ConstantPool::save_and_throw_exception (records LinkageErrors only); not yet
// run):
//   A$L / A$1: every column NCDFE(R21Fz9EmMissing1Gone<-java.lang.ClassNotFoundException)
//   B$L / B$1: every column NCDFE(r21fz9e/Gone<-java.lang.ClassNotFoundException)
//   C$E: every column NCDFE(..Gone..) except declaring=null (no InnerClasses row)
//   D$X ask 1..3: EXC java.lang.IllegalStateException (msg "flaky 1".."flaky 3",
//        not recorded), ask 4 and every later fact: anon=false local=true
//        simple="X" canonical=null enclosing=r21fz9e.Flaky declaring=null
//        method=null
//   F$N: every column NCDFE(nc-custom<-none) (the loader's own LinkageError,
//        recorded and re-thrown)
//   G$W: every column NCDFE(r21fz9e/Wrong<-none) (SystemDictionary drops a
//        loader answer whose name differs and throws NCDFE with the asked name)
//   H$C: anon=false local=true simple="C" canonical=null enclosing=r21fz9e.Host
//        declaring=null method=null
//   every "hot" line prints only "hot <class> done" (same=20000 per fact);
//   arrays: A$L[] / B$L[] / C$E[] simple and canonical NCDFE as the component;
//   "again" lines equal the cold ones;
//   asks {B$1=1, B$L=1, C$E=1, D$X=1, F$N=1, Flaky=4, G$W=1, Gone=3, H$C=1,
//   Host=1, Nc=1, Other=1, Wrong=1}: a refused name is
//   asked once per ASKING class (the LinkageError is recorded in that class's
//   constant pool; B$L, B$1 and C$E each ask Gone once); Flaky's
//   IllegalStateException is not recorded, so it is asked again until it loads.

import java.lang.classfile.ClassElement;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.EnclosingMethodAttribute;
import java.lang.classfile.attribute.InnerClassInfo;
import java.lang.classfile.attribute.InnerClassesAttribute;
import java.lang.constant.ClassDesc;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.TreeMap;
import java.util.function.Supplier;

public class R21Fz9EmMissing1 {
    static final String H = "R21Fz9EmMissing1";
    static final String P = "r21fz9e.";

    static ClassElement row(String inner, String name) {
        return InnerClassesAttribute.of(InnerClassInfo.of(ClassDesc.of(inner),
                Optional.empty(), Optional.ofNullable(name), 0));
    }

    static ClassElement em(String cls) {
        return EnclosingMethodAttribute.of(ClassDesc.of(cls), Optional.empty(), Optional.empty());
    }

    static byte[] cls(String name, ClassElement... attrs) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            for (ClassElement a : attrs) {
                cb.with(a);
            }
        });
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        final Map<String, Integer> asks = new TreeMap<>();

        Loader() {
            super(R21Fz9EmMissing1.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.startsWith(P)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                String simple = name.substring(P.length());
                int n = asks.merge(simple, 1, Integer::sum);
                switch (simple) {
                    case "Gone":
                        throw new ClassNotFoundException(name);
                    case "Flaky":
                        if (n <= 3) {
                            throw new IllegalStateException("flaky " + n);
                        }
                        break;
                    case "Nc":
                        throw new NoClassDefFoundError("nc-custom");
                    case "Wrong":
                        return loadClass(P + "Other", resolve);
                    default:
                        break;
                }
                byte[] b = bytes.get(name);
                if (b == null) {
                    throw new ClassNotFoundException(name);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static String n(Class<?> c) {
        return c == null ? "null" : c.getName();
    }

    static String q(String s) {
        return s == null ? "null" : "\"" + s + "\"";
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (NoClassDefFoundError e) {
            Throwable c = e.getCause();
            return "NCDFE(" + e.getMessage() + "<-" + (c == null ? "none" : c.getClass().getName()) + ")";
        } catch (IncompatibleClassChangeError e) {
            return "ICCE";
        } catch (LinkageError e) {
            return "LE " + e.getClass().getName() + "(" + e.getMessage() + ")";
        } catch (Throwable t) {
            return "EXC " + t.getClass().getName() + "(" + t.getMessage() + ")";
        }
    }

    static final String[] FACTS = {"anon", "local", "member", "simple", "canonical", "declaring", "enclosing",
        "method"};

    static String fact(Class<?> c, int f) {
        switch (f) {
            case 0: return safe(() -> Boolean.toString(c.isAnonymousClass()));
            case 1: return safe(() -> Boolean.toString(c.isLocalClass()));
            case 2: return safe(() -> Boolean.toString(c.isMemberClass()));
            case 3: return safe(() -> q(c.getSimpleName()));
            case 4: return safe(() -> q(c.getCanonicalName()));
            case 5: return safe(() -> n(c.getDeclaringClass()));
            case 6: return safe(() -> n(c.getEnclosingClass()));
            default: return safe(() -> String.valueOf(c.getEnclosingMethod()));
        }
    }

    static String facts(Class<?> c) {
        StringBuilder sb = new StringBuilder();
        for (int f = 0; f < FACTS.length; f++) {
            sb.append(f == 0 ? "" : " ").append(FACTS[f]).append('=').append(fact(c, f));
        }
        return sb.toString();
    }

    static String shortName(Class<?> c) {
        String s = c.getName();
        return s.startsWith(P) ? s.substring(P.length()) : s.substring(H.length());
    }

    static void hot(String label, Class<?> c) {
        for (int f = 0; f < FACTS.length; f++) {
            String want = fact(c, f);
            int same = 0;
            String last = null;
            for (int i = 0; i < 20_000; i++) {
                String got = fact(c, f);
                if (got.equals(want)) {
                    same++;
                } else {
                    last = got;
                }
            }
            if (same != 20_000) {
                System.out.println("hot " + label + " " + FACTS[f] + " same=" + same + " differs: " + last);
            }
        }
        System.out.println("hot " + label + " done");
    }

    public static void main(String[] args) throws Exception {
        Map<String, Class<?>> all = new LinkedHashMap<>();

        // ---- application loader ----
        MethodHandles.Lookup lk = MethodHandles.lookup();
        String gone = H + "Gone";
        all.put("A$L", lk.defineClass(cls(H + "A$L", row(H + "A$L", "L"), em(gone))));
        all.put("A$1", lk.defineClass(cls(H + "A$1", row(H + "A$1", null), em(gone))));

        // ---- custom loader ----
        Loader ld = new Loader();
        ld.bytes.put(P + "B$L", cls(P + "B$L", row(P + "B$L", "L"), em(P + "Gone")));
        ld.bytes.put(P + "B$1", cls(P + "B$1", row(P + "B$1", null), em(P + "Gone")));
        ld.bytes.put(P + "C$E", cls(P + "C$E", em(P + "Gone")));
        ld.bytes.put(P + "D$X", cls(P + "D$X", row(P + "D$X", "X"), em(P + "Flaky")));
        ld.bytes.put(P + "Flaky", cls(P + "Flaky", row(P + "D$X", "X")));
        ld.bytes.put(P + "F$N", cls(P + "F$N", row(P + "F$N", "N"), em(P + "Nc")));
        ld.bytes.put(P + "G$W", cls(P + "G$W", row(P + "G$W", "W"), em(P + "Wrong")));
        ld.bytes.put(P + "Other", cls(P + "Other", row(P + "G$W", "W")));
        ld.bytes.put(P + "H$C", cls(P + "H$C", row(P + "H$C", "C"), em(P + "Host")));
        ld.bytes.put(P + "Host", cls(P + "Host", row(P + "H$C", "C")));
        for (String s : List.of("B$L", "B$1", "C$E", "F$N", "G$W", "H$C")) {
            all.put(s, Class.forName(P + s, false, ld));
        }
        Class<?> dx = Class.forName(P + "D$X", false, ld);

        // ---- flaky: four single asks, in order ----
        for (int k = 1; k <= 4; k++) {
            System.out.println("D$X ask " + k + " anon=" + fact(dx, 0));
        }
        all.put("D$X", dx);

        // ---- cold ----
        for (Map.Entry<String, Class<?>> e : all.entrySet()) {
            System.out.println("cold " + e.getKey() + " name=" + shortName(e.getValue()) + " "
                    + facts(e.getValue()));
        }
        // ---- arrays of the refused classes ----
        for (String s : List.of("A$L", "B$L", "C$E")) {
            Class<?> arr = java.lang.reflect.Array.newInstance(all.get(s), 0).getClass();
            System.out.println("array " + s + "[] simple=" + safe(() -> q(arr.getSimpleName()))
                    + " canonical=" + safe(() -> q(arr.getCanonicalName())));
        }
        // ---- hot ----
        for (Map.Entry<String, Class<?>> e : all.entrySet()) {
            hot(e.getKey(), e.getValue());
        }
        // ---- again, after hot ----
        List<String> again = new ArrayList<>();
        for (Map.Entry<String, Class<?>> e : all.entrySet()) {
            again.add(e.getKey() + " " + facts(e.getValue()));
        }
        for (String s : again) {
            System.out.println("again " + s);
        }
        System.out.println("asks " + ld.asks);
    }
}
