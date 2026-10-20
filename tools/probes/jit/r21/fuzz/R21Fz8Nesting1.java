// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 22, lane fuzz8: the nesting answers -- isAnonymousClass,
// isLocalClass, isMemberClass, getSimpleName, getCanonicalName,
// getDeclaringClass, getEnclosingClass -- of javac local, anonymous, member
// and lambda classes, of hidden classes (one from javac top-level bytes, three
// built with InnerClasses / EnclosingMethod rows naming the host, which does
// not list them), and of ordinary classfile-built classes whose InnerClasses
// and EnclosingMethod attributes disagree in ways R21Fz6InnerRows1 and
// R21Fz7InnerRowsEm1 do not cover. Each fact is asked cold, then 20000 times
// in a loop (the answers must not change once compiled or memoized).
//
// Risk targeted: wave 21 item 7 (the --compatible name bridges ask the nesting
// kind first; they now skip the declaring-class check for local, anonymous
// and hidden classes), wave 18-19 (first-row member rule, EnclosingMethod
// nesting, CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD), the sticky
// outer-resolution memo, and the hidden self-name rule (a hidden class's own
// InnerClasses row names this_class, which HotSpot rewrites to the hidden name
// in ClassFileParser, so the row still matches the hidden class).
//
// Row legend for the classfile-built classes (package r21fz8n, one loader):
//   Y$1  EM(Y), no InnerClasses at all                -> anonymous (no row, no name)
//   Z$2  row (Z$2, Z, Two) + EM(Z); Z lists the row   -> local, AND declaring = Z
//   W$3  row (W$3, W, Three), no EM; W lists the row  -> member (control)
//   U$5  rows (U$5, -, Five) then (U$5, U, Fiver), no EM; U lists the 2nd -> first row wins: top level
//   T$6  row (T$6, T, Six) + EM(S); T lists the row   -> local, enclosing S, declaring T
// Hidden (default package, host = this class, which lists none of them):
//   HTop   javac top-level bytes (R21Fz8NsTop)
//   HLoc   row (self, -, HLoc) + EM(host)             -> local; declaring ICCE
//   HAnon  row (self, -, -)   + EM(host)              -> anonymous; declaring ICCE
//   HMem   row (self, host, HMem), no EM              -> spoofed member; ICCE where asked
//
// Hidden names are printed with "/0x<hex>" replaced by "/0xH"; exception
// messages are not printed.
//
// Run:
//   javac -d out R21Fz8Nesting1.java
//   java -Xmx256m -cp out R21Fz8Nesting1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz8Nesting1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz8Nesting1
//   cratonvm --nojit -Xmx256m -cp out R21Fz8Nesting1
// HotSpot expectation (by reading JDK 25 Class.java and HotSpot's
// compute_enclosing_class / find_inner_classes_attr / check_for_inner_class;
// not yet run), as anon/local/member simple canonical declaring enclosing
// (Loc is javac's R21Fz8Nesting1$1Loc; names in the declaring / enclosing
// columns are printed as binary names, r21fz8n. stripped):
//   Mem    false/false/true  "Mem"   "R21Fz8Nesting1.Mem" R21Fz8Nesting1 R21Fz8Nesting1
//   Loc    false/true/false  "Loc"   null  null  R21Fz8Nesting1
//   Inner  false/false/true  "Inner" null  Loc   Loc        (member of a local class)
//   Anon   true/false/false  ""      null  null  R21Fz8Nesting1
//   InLam  false/true/false  "InLam" null  null  R21Fz8Nesting1
//   Lambda false/false/false "R21Fz8Nesting1$$Lambda/0xH" null null null
//   HTop   false/false/false "R21Fz8NsTop/0xH" null null null
//   HLoc   false/true/false  "HLoc"  null  ICCE  R21Fz8Nesting1
//   HAnon  true/false/false  ""      null  ICCE  R21Fz8Nesting1
//   HMem   false/false/ICCE  ICCE    null  ICCE  ICCE   (getCanonicalName asks isHidden first)
//   Y$1    true/false/false  ""      null  null  Y
//   Z$2    false/true/false  "Two"   null  Z     Z
//   W$3    false/false/true  "Three" "r21fz8n.W.Three" W W
//   U$5    false/false/false "U$5"   "r21fz8n.U$5" null null
//   T$6    false/true/false  "Six"   null  T     S
// and every "hot" line reads same=20000.
// Y$1 declaring=null relies on compute_enclosing_class consulting
// EnclosingMethod only when an InnerClasses row for the class exists. U$5's
// two rows share an inner_class_info_index; whether HotSpot takes the first
// row or drops the attribute (check_inner_classes_circularity), the answers
// above are the same: only a "last row wins" reading gives member=true.

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
import java.util.function.Supplier;

public class R21Fz8Nesting1 {
    static final String P = "r21fz8n.";
    static final ClassDesc HOST = ClassDesc.of("R21Fz8Nesting1");
    static final int PS = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;

    static class Mem {
    }

    static Class<?>[] localAndInner() {
        class Loc {
            class Inner {
            }
        }
        return new Class<?>[] {Loc.class, Loc.Inner.class};
    }

    static Class<?> anon() {
        Object o = new Object() {
        };
        return o.getClass();
    }

    static Class<?> inLambda() {
        Supplier<Class<?>> s = () -> {
            class InLam {
            }
            return InLam.class;
        };
        return s.get();
    }

    static Class<?> lambda() {
        Runnable r = () -> {
        };
        return r.getClass();
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(R21Fz8Nesting1.class.getClassLoader());
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
                byte[] b = bytes.get(name);
                if (b == null) {
                    throw new ClassNotFoundException(name);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static ClassDesc d(String simple) {
        return ClassDesc.of(P + simple);
    }

    static InnerClassInfo info(ClassDesc inner, ClassDesc outer, String name) {
        return InnerClassInfo.of(inner, Optional.ofNullable(outer), Optional.ofNullable(name), PS);
    }

    static ClassElement rows(InnerClassInfo... infos) {
        return InnerClassesAttribute.of(infos);
    }

    static ClassElement em(ClassDesc outer) {
        return EnclosingMethodAttribute.of(outer, Optional.empty(), Optional.empty());
    }

    static byte[] cls(ClassDesc name, ClassElement... attrs) {
        return ClassFile.of().build(name, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            for (ClassElement a : attrs) {
                cb.with(a);
            }
        });
    }

    static String norm(String s) {
        return s == null ? "null" : s.replaceAll("/0x[0-9a-fA-F]+", "/0xH");
    }

    static String n(Class<?> c) {
        if (c == null) {
            return "null";
        }
        String s = c.getName();
        return norm(s.startsWith(P) ? s.substring(P.length()) : s);
    }

    static String q(String s) {
        return s == null ? "null" : "\"" + norm(s) + "\"";
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (IncompatibleClassChangeError e) {
            return "ICCE";
        } catch (NoClassDefFoundError e) {
            return "NCDFE";
        } catch (Throwable t) {
            return "EXC " + t.getClass().getName();
        }
    }

    static String facts(Class<?> c) {
        return "anon=" + safe(() -> Boolean.toString(c.isAnonymousClass()))
                + " local=" + safe(() -> Boolean.toString(c.isLocalClass()))
                + " member=" + safe(() -> Boolean.toString(c.isMemberClass()))
                + " simple=" + safe(() -> q(c.getSimpleName()))
                + " canonical=" + safe(() -> q(c.getCanonicalName()))
                + " declaring=" + safe(() -> n(c.getDeclaringClass()))
                + " enclosing=" + safe(() -> n(c.getEnclosingClass()));
    }

    public static void main(String[] args) throws Throwable {
        Map<String, Class<?>> all = new LinkedHashMap<>();
        all.put("Mem", Mem.class);
        Class<?>[] li = localAndInner();
        all.put("Loc", li[0]);
        all.put("Inner", li[1]);
        all.put("Anon", anon());
        all.put("InLam", inLambda());
        all.put("Lambda", lambda());

        MethodHandles.Lookup lk = MethodHandles.lookup();
        byte[] top;
        try (var in = R21Fz8Nesting1.class.getResourceAsStream("/R21Fz8NsTop.class")) {
            top = in.readAllBytes();
        }
        all.put("HTop", lk.defineHiddenClass(top, false).lookupClass());
        ClassDesc hl = ClassDesc.of("R21Fz8NsHLoc");
        all.put("HLoc", lk.defineHiddenClass(cls(hl, rows(info(hl, null, "HLoc")), em(HOST)), false)
                .lookupClass());
        ClassDesc ha = ClassDesc.of("R21Fz8NsHAnon");
        all.put("HAnon", lk.defineHiddenClass(cls(ha, rows(info(ha, null, null)), em(HOST)), false)
                .lookupClass());
        ClassDesc hm = ClassDesc.of("R21Fz8NsHMem");
        all.put("HMem", lk.defineHiddenClass(cls(hm, rows(info(hm, HOST, "HMem"))), false).lookupClass());

        Loader ld = new Loader();
        ld.bytes.put(P + "Y", cls(d("Y")));
        ld.bytes.put(P + "Y$1", cls(d("Y$1"), em(d("Y"))));
        ld.bytes.put(P + "Z", cls(d("Z"), rows(info(d("Z$2"), d("Z"), "Two"))));
        ld.bytes.put(P + "Z$2", cls(d("Z$2"), rows(info(d("Z$2"), d("Z"), "Two")), em(d("Z"))));
        ld.bytes.put(P + "W", cls(d("W"), rows(info(d("W$3"), d("W"), "Three"))));
        ld.bytes.put(P + "W$3", cls(d("W$3"), rows(info(d("W$3"), d("W"), "Three"))));
        ld.bytes.put(P + "U", cls(d("U"), rows(info(d("U$5"), d("U"), "Fiver"))));
        ld.bytes.put(P + "U$5", cls(d("U$5"),
                rows(info(d("U$5"), null, "Five"), info(d("U$5"), d("U"), "Fiver"))));
        ld.bytes.put(P + "S", cls(d("S")));
        ld.bytes.put(P + "T", cls(d("T"), rows(info(d("T$6"), d("T"), "Six"))));
        ld.bytes.put(P + "T$6", cls(d("T$6"), rows(info(d("T$6"), d("T"), "Six")), em(d("S"))));
        for (String s : List.of("Y$1", "Z$2", "W$3", "U$5", "T$6")) {
            all.put(s, Class.forName(P + s, false, ld));
        }

        List<String> cold = new ArrayList<>();
        for (Map.Entry<String, Class<?>> e : all.entrySet()) {
            String f = facts(e.getValue());
            cold.add(f);
            System.out.println(e.getKey() + " name=" + n(e.getValue()) + " " + f);
        }
        int k = 0;
        for (Map.Entry<String, Class<?>> e : all.entrySet()) {
            String want = cold.get(k++);
            Class<?> c = e.getValue();
            int same = 0;
            String last = want;
            for (int i = 0; i < 20000; i++) {
                String f = facts(c);
                if (f.equals(want)) {
                    same++;
                } else {
                    last = f;
                }
            }
            System.out.println("hot " + e.getKey() + " same=" + same
                    + (same == 20000 ? "" : " differs: " + last));
        }
    }
}

// The bytes of the hidden class HTop: a plain top-level class.
class R21Fz8NsTop {
}
