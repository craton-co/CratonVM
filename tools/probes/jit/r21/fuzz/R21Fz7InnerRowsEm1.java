// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 21, lane review-w21: the nesting answers of local and
// anonymous classes (`EnclosingMethod` present) whose `InnerClasses` self row
// names an outer class that does NOT list them, or cannot be loaded.
//
// Risk targeted (docs/known-issues/jit/r21-review-w21-findings-20261019.md,
// item 7): JDK 25 reaches `getDeclaringClass0` -- and with it HotSpot's
// `compute_enclosing_class` resolution and `check_for_inner_class` agreement
// check -- only from `getDeclaringClass`, `isMemberClass` and
// `getEnclosingClass` of a class WITHOUT `EnclosingMethod`, and from
// `getSimpleName` through `isTopLevelClass`, which short-circuits on
// `isLocalOrAnonymousClass` (= `hasEnclosingMethodInfo`). `getCanonicalName`
// answers null for a local or anonymous class before it asks anything else.
// The `--compatible` `getSimpleName` / `getCanonicalName` bridges ask the
// declaring-class agreement (`inner_classes_icce_for_mirror`) and the outer
// resolution error (`name_bridge_outer_resolution_error`) BEFORE the nesting
// kind (`class_nesting_of`), so they may throw where HotSpot answers.
//
// Classes (package r21fz7, one loader; X lists none of them):
//   X$7   row (X$7, X, Seven)  + EM(X)   local; X disagrees
//   X$8   row (X$8, X, -)      + EM(X)   anonymous; X disagrees
//   X$9   row (X$9, Gone, Nine) + EM(X)  local; Gone does not exist
//   X$10  row (X$10, X, Ten), no EM      member; X disagrees (control)
//
// Run:
//   javac -d out R21Fz7InnerRowsEm1.java
//   java -cp out R21Fz7InnerRowsEm1                     # HotSpot 25
//   cratonvm -cp out R21Fz7InnerRowsEm1                 # --jdk-only
//   cratonvm --compatible -cp out R21Fz7InnerRowsEm1
// HotSpot expectation (by reading JDK 25 Class.java and HotSpot's
// compute_enclosing_class / check_for_inner_class, not yet run), as
// anon/local/member simple canonical declaring enclosing:
//   X$7  false/true/false  "Seven" null  ICCE  X
//   X$8  true/false/false  ""      null  ICCE  X
//   X$9  false/true/false  "Nine"  null  NCDFE X
//   X$10 false/false/ICCE  ICCE    ICCE  ICCE  ICCE
// The "again" rows repeat X$9 (a recorded resolution failure stays the same
// error) and X$7.

import java.lang.classfile.ClassElement;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.EnclosingMethodAttribute;
import java.lang.classfile.attribute.InnerClassInfo;
import java.lang.classfile.attribute.InnerClassesAttribute;
import java.lang.constant.ClassDesc;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.function.Supplier;

public class R21Fz7InnerRowsEm1 {
    static final String P = "r21fz7.";

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(R21Fz7InnerRowsEm1.class.getClassLoader());
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

    static ClassElement row(String inner, String outer, String name) {
        return InnerClassesAttribute.of(InnerClassInfo.of(d(inner),
                Optional.ofNullable(outer).map(R21Fz7InnerRowsEm1::d), Optional.ofNullable(name),
                ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC));
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
        return c == null ? "null" : c.getName().substring(P.length());
    }

    static String q(String s) {
        return s == null ? "null" : "\"" + s + "\"";
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
        return c.getName().substring(P.length())
                + " anon=" + safe(() -> Boolean.toString(c.isAnonymousClass()))
                + " local=" + safe(() -> Boolean.toString(c.isLocalClass()))
                + " member=" + safe(() -> Boolean.toString(c.isMemberClass()))
                + " simple=" + safe(() -> q(c.getSimpleName()))
                + " canonical=" + safe(() -> q(c.getCanonicalName()))
                + " declaring=" + safe(() -> n(c.getDeclaringClass()))
                + " enclosing=" + safe(() -> n(c.getEnclosingClass()));
    }

    public static void main(String[] args) throws Exception {
        Loader ld = new Loader();
        ld.bytes.put(P + "X", cls("X"));
        ld.bytes.put(P + "X$7", cls("X$7", row("X$7", "X", "Seven"), em()));
        ld.bytes.put(P + "X$8", cls("X$8", row("X$8", "X", null), em()));
        ld.bytes.put(P + "X$9", cls("X$9", row("X$9", "Gone", "Nine"), em()));
        ld.bytes.put(P + "X$10", cls("X$10", row("X$10", "X", "Ten")));
        Class.forName(P + "X", false, ld);
        for (String s : List.of("X$7", "X$8", "X$9", "X$10")) {
            Class<?> c;
            try {
                c = Class.forName(P + s, false, ld);
            } catch (Throwable t) {
                System.out.println(s + " define EXC " + t.getClass().getName());
                continue;
            }
            System.out.println(facts(c));
        }
        for (String s : List.of("X$9", "X$7")) {
            System.out.println("again " + facts(Class.forName(P + s, false, ld)));
        }
    }
}
