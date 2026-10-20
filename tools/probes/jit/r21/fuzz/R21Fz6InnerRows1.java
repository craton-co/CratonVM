// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 18, lane review-w18: the nesting answers of hand-built
// `InnerClasses` / `EnclosingMethod` shapes javac never writes
// (`CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW`, wave 16:
// `lang_class::declaring_member_row_outer`, `row_nesting_kind`).
//
// Risk targeted (docs/known-issues/jit/r21-review-w18-findings-20261019.md):
// HotSpot decides every nesting question from the FIRST `InnerClasses` row
// naming the class (`find_inner_classes_attr`) and from `EnclosingMethod`:
//   isLocalOrAnonymousClass = hasEnclosingMethodInfo (EnclosingMethod alone);
//   getDeclaringClass0      = that row's outer class when it has one
//                             (`compute_enclosing_class`, named or not);
//   isTopLevelClass         = neither, so getSimpleName is the binary-name
//                             tail and getCanonicalName is getName().
// Several CratonVM walks read a row's SHAPE instead, or search past the first
// row: the compatible-mode nesting kind, the getEnclosingClass bridge, and the
// `Lookup.in` outermost-class walk. The last line of each class is the
// `Lookup.in` answer of a private lookup on the class asked for the outer
// class X: 15 when the two share an outermost class, 9 when they do not.
//
// Classes (package r21fz6, one loader; X lists the rows of X$1, X$3, X$6):
//   X$1  row (X$1, X, -)                  member, no simple name
//   X$2  row (X$2, -, -), no EM           top level to HotSpot
//   X$3  row (X$3, X, Three) + EM(X)      local (EM decides), declaring X
//   X$4  no InnerClasses, EM(X)           anonymous (EM alone)
//   X$5  row (X$5, -, Loc), no EM         top level to HotSpot
//   X$6  rows (X$6, -, -), (X$6, X, Six)  the FIRST row decides: top level
// (X$6's member row comes second on purpose: HotSpot's circularity check
// ignores the attribute when a both-indices row is followed by another row of
// the same inner class.)
//
// Run:
//   javac -d out R21Fz6InnerRows1.java
//   java -cp out R21Fz6InnerRows1                     # HotSpot 25
//   cratonvm -cp out R21Fz6InnerRows1                 # --jdk-only
//   cratonvm --compatible -cp out R21Fz6InnerRows1
//   CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW=0 cratonvm ...   # control arm
// HotSpot expectation (by reading JDK 25 and HotSpot, not yet run), as
// anon/local/member simple canonical declaring enclosing in(X):
//   X$1 false/false/true  ""    "r21fz6.X."   X    X    15
//   X$2 false/false/false "X$2" "r21fz6.X$2"  null null 9
//   X$3 false/true/false  Three null          X    X    15
//   X$4 true/false/false  ""    null          null X    15
//   X$5 false/false/false "X$5" "r21fz6.X$5"  null null 9
//   X$6 false/false/false "X$6" "r21fz6.X$6"  null null 9

import java.lang.classfile.ClassElement;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.EnclosingMethodAttribute;
import java.lang.classfile.attribute.InnerClassInfo;
import java.lang.classfile.attribute.InnerClassesAttribute;
import java.lang.constant.ClassDesc;
import java.lang.invoke.MethodHandles;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.function.Supplier;

public class R21Fz6InnerRows1 {
    static final String P = "r21fz6.";

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(R21Fz6InnerRows1.class.getClassLoader());
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

    static InnerClassInfo row(String inner, String outer, String name) {
        return InnerClassInfo.of(d(inner), Optional.ofNullable(outer).map(R21Fz6InnerRows1::d),
                Optional.ofNullable(name), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
    }

    static ClassElement rows(InnerClassInfo... rows) {
        return InnerClassesAttribute.of(rows);
    }

    static ClassElement em() {
        return EnclosingMethodAttribute.of(d("X"), Optional.empty(), Optional.empty());
    }

    static byte[] cls(String name, ClassElement... attrs) {
        return ClassFile.of().build(d(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            for (ClassElement a : attrs) {
                cb.with(a);
            }
        });
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
        } catch (Throwable t) {
            return "EXC " + t.getClass().getName();
        }
    }

    static String facts(Class<?> c, Class<?> x) {
        String in = safe(() -> {
            try {
                return Integer.toString(
                        MethodHandles.privateLookupIn(c, MethodHandles.lookup()).in(x).lookupModes());
            } catch (IllegalAccessException e) {
                return "IAE";
            }
        });
        return c.getName().substring(P.length())
                + " anon=" + safe(() -> Boolean.toString(c.isAnonymousClass()))
                + " local=" + safe(() -> Boolean.toString(c.isLocalClass()))
                + " member=" + safe(() -> Boolean.toString(c.isMemberClass()))
                + " simple=" + safe(() -> q(c.getSimpleName()))
                + " canonical=" + safe(() -> q(c.getCanonicalName()))
                + " declaring=" + safe(() -> n(c.getDeclaringClass()))
                + " enclosing=" + safe(() -> n(c.getEnclosingClass()))
                + " in(X)=" + in;
    }

    public static void main(String[] args) throws Exception {
        Loader ld = new Loader();
        ld.bytes.put(P + "X", cls("X",
                rows(row("X$1", "X", null), row("X$3", "X", "Three"), row("X$6", "X", "Six"))));
        ld.bytes.put(P + "X$1", cls("X$1", rows(row("X$1", "X", null))));
        ld.bytes.put(P + "X$2", cls("X$2", rows(row("X$2", null, null))));
        ld.bytes.put(P + "X$3", cls("X$3", rows(row("X$3", "X", "Three")), em()));
        ld.bytes.put(P + "X$4", cls("X$4", em()));
        ld.bytes.put(P + "X$5", cls("X$5", rows(row("X$5", null, "Loc"))));
        ld.bytes.put(P + "X$6", cls("X$6", rows(row("X$6", null, null), row("X$6", "X", "Six"))));
        Class<?> x = Class.forName(P + "X", false, ld);
        for (String s : List.of("X$1", "X$2", "X$3", "X$4", "X$5", "X$6")) {
            Class<?> c;
            try {
                c = Class.forName(P + s, false, ld);
            } catch (Throwable t) {
                System.out.println(s + " define EXC " + t.getClass().getName());
                continue;
            }
            System.out.println(facts(c, x));
        }
        // The same questions again, after every class's answers were asked
        // once (the VM's name and verdict memos are warm).
        for (String s : List.of("X$2", "X$6")) {
            System.out.println("again " + facts(Class.forName(P + s, false, ld), x));
        }
    }
}
