// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 22, lane review-w22: the R21Fz7InnerRowsEm1 shapes
// (local and member classes whose InnerClasses self row names an outer class
// that does not list them, or does not exist), defined in the APPLICATION
// loader with Lookup.defineClass instead of a user-defined loader.
//
// Why a second copy (docs/known-issues/jit/r21-review-w22-findings-20261019.md,
// item 1, "what wave 21 fixed, and where"): the --compatible bridge check
// that raises the outer class's resolution error,
// `name_bridge_outer_resolution_error`, returns early whenever ANY
// user-defined loader is registered in the VM. R21Fz7InnerRowsEm1 builds its
// classes in a user loader, so it never reaches that check, and its X$9 row
// cannot show the wave-21 change to it. This probe makes no ClassLoader of
// its own, so the check runs, and wave 21's `name_bridge_asks_declaring_class`
// decides X$9's simple name.
//
// Classes (default package, application loader; the host lists none):
//   $7   row ($7, host, "Seven") + EM(host)   local; host disagrees
//   $9   row ($9, Gone, "Nine")  + EM(host)   local; Gone does not exist
//   $10  row ($10, host, "Ten"), no EM        member; host disagrees (control)
// host = R21Fz8ReviewAppRows1, Gone = R21Fz8ReviewAppRowsGone.
//
// Run:
//   javac -d out R21Fz8ReviewAppRows1.java
//   java -cp out R21Fz8ReviewAppRows1                     # HotSpot 25
//   cratonvm -cp out R21Fz8ReviewAppRows1                 # --jdk-only
//   cratonvm --compatible -cp out R21Fz8ReviewAppRows1
//
// HotSpot expectation (by reading JDK 25 Class.java and HotSpot's
// compute_enclosing_class / check_for_inner_class; not yet run).
// isTopLevelClass short-circuits on EnclosingMethod, and getCanonicalName
// answers null for a local class before getEnclosingClass, so only
// getDeclaringClass reaches the member row:
//   $7  anon=false local=true member=false simple="Seven" canonical=null
//       declaring=ICCE enclosing=R21Fz8ReviewAppRows1
//   $9  anon=false local=true member=false simple="Nine" canonical=null
//       declaring=NCDFE enclosing=R21Fz8ReviewAppRows1
//   $10 anon=false local=false member=ICCE simple=ICCE canonical=ICCE
//       declaring=ICCE enclosing=ICCE
//   again $9: as $9 (the resolution error of Gone is recorded; same error)
// Expected on CratonVM after wave 21: all modes match. Before wave 21,
// --compatible threw NCDFE from $9's getSimpleName (and possibly
// getCanonicalName), through the outer-resolution check.

import java.lang.classfile.ClassElement;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.EnclosingMethodAttribute;
import java.lang.classfile.attribute.InnerClassInfo;
import java.lang.classfile.attribute.InnerClassesAttribute;
import java.lang.constant.ClassDesc;
import java.lang.invoke.MethodHandles;
import java.util.function.Supplier;
import java.util.Optional;

public class R21Fz8ReviewAppRows1 {
    static final String H = "R21Fz8ReviewAppRows1";
    static final String GONE = "R21Fz8ReviewAppRowsGone";

    static ClassElement row(String inner, String outer, String name) {
        return InnerClassesAttribute.of(InnerClassInfo.of(ClassDesc.of(inner),
                Optional.of(ClassDesc.of(outer)), Optional.of(name),
                ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC));
    }

    static ClassElement em() {
        return EnclosingMethodAttribute.of(ClassDesc.of(H), Optional.empty(), Optional.empty());
    }

    static byte[] cls(String name, ClassElement... attrs) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
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
        } catch (IncompatibleClassChangeError e) {
            return "ICCE";
        } catch (NoClassDefFoundError e) {
            return "NCDFE";
        } catch (Throwable t) {
            return "EXC " + t.getClass().getName();
        }
    }

    static String facts(Class<?> c) {
        return c.getName().substring(H.length())
                + " anon=" + safe(() -> Boolean.toString(c.isAnonymousClass()))
                + " local=" + safe(() -> Boolean.toString(c.isLocalClass()))
                + " member=" + safe(() -> Boolean.toString(c.isMemberClass()))
                + " simple=" + safe(() -> q(c.getSimpleName()))
                + " canonical=" + safe(() -> q(c.getCanonicalName()))
                + " declaring=" + safe(() -> n(c.getDeclaringClass()))
                + " enclosing=" + safe(() -> n(c.getEnclosingClass()));
    }

    public static void main(String[] args) throws Exception {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Class<?> c7 = lk.defineClass(cls(H + "$7", row(H + "$7", H, "Seven"), em()));
        Class<?> c9 = lk.defineClass(cls(H + "$9", row(H + "$9", GONE, "Nine"), em()));
        Class<?> c10 = lk.defineClass(cls(H + "$10", row(H + "$10", H, "Ten")));
        System.out.println(facts(c7));
        System.out.println(facts(c9));
        System.out.println(facts(c10));
        System.out.println("again " + facts(c9));
    }
}
