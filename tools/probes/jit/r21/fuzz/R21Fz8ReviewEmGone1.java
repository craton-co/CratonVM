// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 22, lane review-w22: the nesting answers of local and
// anonymous classes whose EnclosingMethod class CANNOT be loaded, defined in
// the APPLICATION loader (Lookup.defineClass; no user-defined loader exists,
// so the --compatible bridges' "no user loader" arm is the one exercised).
//
// Risk targeted (docs/known-issues/jit/r21-review-w22-findings-20261019.md,
// item 1): wave 21's `name_bridge_asks_declaring_class` makes the
// --compatible getSimpleName / getCanonicalName bridges skip every
// declaring-class check for a class with EnclosingMethod. But JDK 25's
// `isLocalOrAnonymousClass()` is `hasEnclosingMethodInfo()`, which calls
// `getEnclosingMethod0()`, and HotSpot's JVM_GetEnclosingMethodInfo resolves
// the EnclosingMethod class with `klass_at(.., CHECK_NULL)`. So a missing
// EnclosingMethod class is a NoClassDefFoundError out of getSimpleName and
// getCanonicalName (and isAnonymousClass, isLocalClass, isMemberClass,
// getEnclosingClass, getEnclosingMethod). Before wave 21 the bridges raised
// it through `name_bridge_outer_resolution_error` (for a self row with no
// outer class); after wave 21 they answer a name.
//
// Classes (default package, application loader):
//   $L  row ($L, -, "L") + EM(R21Fz8ReviewEmGoneMissing)   local
//   $1  row ($1, -, -)   + EM(R21Fz8ReviewEmGoneMissing)   anonymous
//   $E  no InnerClasses  + EM(R21Fz8ReviewEmGoneMissing)   EM only
//   $C  row ($C, -, "C") + EM(R21Fz8ReviewEmGone1)         control (EM loads)
//   $L[] (an array of $L): simple and canonical only
//
// Run:
//   javac -d out R21Fz8ReviewEmGone1.java
//   java -cp out R21Fz8ReviewEmGone1                     # HotSpot 25
//   cratonvm -cp out R21Fz8ReviewEmGone1                 # --jdk-only
//   cratonvm --compatible -cp out R21Fz8ReviewEmGone1
//
// HotSpot expectation (by reading JDK 25 Class.java and HotSpot's
// JVM_GetEnclosingMethodInfo / compute_enclosing_class /
// check_for_inner_class; not yet run). NCDFE is
// "NoClassDefFoundError: R21Fz8ReviewEmGoneMissing" caused by
// ClassNotFoundException (the resolution error is recorded once and re-thrown
// with its cause on every later ask):
//   $L  anon=NCDFE local=NCDFE member=NCDFE simple=NCDFE canonical=NCDFE
//       declaring=NCDFE enclosing=NCDFE method=NCDFE
//   $1  the same as $L
//   $E  anon=NCDFE local=NCDFE member=NCDFE simple=NCDFE canonical=NCDFE
//       declaring=null (no InnerClasses: compute_enclosing_class asks nothing)
//       enclosing=NCDFE method=NCDFE
//   $C  anon=false local=true member=false simple="C" canonical=null
//       declaring=ICCE (the host does not list $C) enclosing=R21Fz8ReviewEmGone1
//       method=null
//   $L[] simple=NCDFE canonical=NCDFE
//   again $L simple=NCDFE canonical=NCDFE
// Expected on CratonVM: --jdk-only matches (the JDK body calls the
// getEnclosingMethod0 native, which resolves through the checked door);
// --compatible very likely prints "L" / null / "" for the simple and
// canonical columns of $L, $1, $E and $L[].

import java.lang.classfile.ClassElement;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.EnclosingMethodAttribute;
import java.lang.classfile.attribute.InnerClassInfo;
import java.lang.classfile.attribute.InnerClassesAttribute;
import java.lang.constant.ClassDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.function.Supplier;

public class R21Fz8ReviewEmGone1 {
    static final String H = "R21Fz8ReviewEmGone1";
    static final String MISSING = "R21Fz8ReviewEmGoneMissing";

    static ClassElement row(String inner, String name) {
        return InnerClassesAttribute.of(InnerClassInfo.of(ClassDesc.of(inner),
                Optional.empty(), Optional.ofNullable(name), 0));
    }

    static ClassElement em(String cls) {
        return EnclosingMethodAttribute.of(ClassDesc.of(cls), Optional.empty(), Optional.empty());
    }

    static byte[] cls(String name, ClassElement... attrs) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_SUPER);
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
            Throwable c = e.getCause();
            return "NCDFE(" + e.getMessage() + "<-"
                    + (c == null ? "none" : c.getClass().getName()) + ")";
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
                + " enclosing=" + safe(() -> n(c.getEnclosingClass()))
                + " method=" + safe(() -> String.valueOf(c.getEnclosingMethod()));
    }

    public static void main(String[] args) throws Exception {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        List<Class<?>> made = new ArrayList<>();
        String[][] shapes = {
            {"$L", "L", MISSING, "row"},
            {"$1", null, MISSING, "row"},
            {"$E", null, MISSING, "none"},
            {"$C", "C", H, "row"},
        };
        for (String[] s : shapes) {
            String name = H + s[0];
            byte[] b = "row".equals(s[3])
                    ? cls(name, row(name, s[1]), em(s[2]))
                    : cls(name, em(s[2]));
            try {
                made.add(lk.defineClass(b));
            } catch (Throwable t) {
                System.out.println(s[0] + " define EXC " + t.getClass().getName());
                made.add(null);
            }
        }
        for (Class<?> c : made) {
            if (c != null) {
                System.out.println(facts(c));
            }
        }
        Class<?> l = made.get(0);
        if (l != null) {
            Class<?> arr = Array.newInstance(l, 0).getClass();
            System.out.println("$L[] simple=" + safe(() -> q(arr.getSimpleName()))
                    + " canonical=" + safe(() -> q(arr.getCanonicalName())));
            System.out.println("again $L simple=" + safe(() -> q(l.getSimpleName()))
                    + " canonical=" + safe(() -> q(l.getCanonicalName())));
        }
    }
}
