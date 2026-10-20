// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 23, lane review-w23: the --compatible name bridges'
// EnclosingMethod resolution (wave 22, `name_bridge_enclosing_method_error`)
// on the arm R21Fz8ReviewEmGone1 never reaches: classes defined by a
// USER-DEFINED loader, an EnclosingMethod class that exists but fails to load
// (its superclass is missing), the JVMS 5.4.3 stickiness once the missing
// class becomes loadable, and a hidden class (application loader) whose
// EnclosingMethod class is missing.
//
// Risk targeted (docs/known-issues/jit/r21-review-w23-findings-20261019.md,
// item 8): the new function is not gated on "no user-defined loader" (the old
// `name_bridge_outer_resolution_error` returns early once one exists), so this
// is its first exercise through a user loader's loadClass; the error of a
// FAILED LOAD (not a missing class) must be the loader's NoClassDefFoundError
// for the superclass, recorded and re-thrown with its cause; and the loader
// must be asked once per constant-pool entry, never again.
//
// Classes (loader L, parent = platform loader; findClass counts its asks):
//   Fz9EmHost$L  row (self, no outer, "L") + EM(Fz9EmGone)   Fz9EmGone missing
//   Fz9EmHost$B  row (self, no outer, "B") + EM(Fz9EmBad)    Fz9EmBad extends
//                                                             Fz9EmGoneSuper (missing)
//   Fz9EmHost$C  row (self, no outer, "C") + EM(Fz9EmHost)   control, EM loads
//   R21Fz9ReviewEmUser1$H  hidden (Lookup.defineHiddenClass, app loader),
//                          EM(R21Fz9ReviewEmUser1Missing), no InnerClasses
//
// Run:
//   javac -d out R21Fz9ReviewEmUser1.java
//   java -cp out R21Fz9ReviewEmUser1                     # HotSpot 25
//   cratonvm -cp out R21Fz9ReviewEmUser1                 # --jdk-only
//   cratonvm --compatible -cp out R21Fz9ReviewEmUser1
//
// HotSpot expectation (by reading JDK 25 Class.java: getSimpleName0 ->
// isTopLevelClass -> hasEnclosingMethodInfo -> getEnclosingMethod0, and
// HotSpot JVM_GetEnclosingMethodInfo's klass_at(.., CHECK_NULL) with the
// resolution-error table; ClassFileParser keeps EnclosingMethod for a hidden
// class, which is the least certain line; not yet run):
//   $L simple=NCDFE(Fz9EmGone<-java.lang.ClassNotFoundException: Fz9EmGone)
//   $L canonical=<the same>
//   $L local=<the same>
//   $L[] simple=<the same> canonical=<the same>
//   $B simple=NCDFE(Fz9EmGoneSuper<-java.lang.ClassNotFoundException: Fz9EmGoneSuper)
//   $B canonical=<the same>
//   $B again=<the same>
//   $C simple="C" canonical=null local=true
//   $H simple=NCDFE(R21Fz9ReviewEmUser1Missing<-java.lang.ClassNotFoundException: R21Fz9ReviewEmUser1Missing)
//   $H canonical=null
//   asks gone=1 goneSuper=1
//   now loadable: Fz9EmGone asks gone=2
//   $L after simple=NCDFE(Fz9EmGone<-java.lang.ClassNotFoundException: Fz9EmGone)
//   $L after canonical=<the same>
//   asks gone=2 goneSuper=1
// Expected on CratonVM: --jdk-only matches (JDK bodies, the checked
// getEnclosingMethod0 door). --compatible: should match since wave 22; the
// lines most at risk are $B (the failed load's error and cause through the
// user loader), the asks counts (stickiness), and $H simple.

import java.lang.classfile.ClassElement;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.EnclosingMethodAttribute;
import java.lang.classfile.attribute.InnerClassInfo;
import java.lang.classfile.attribute.InnerClassesAttribute;
import java.lang.constant.ClassDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Array;
import java.util.HashMap;
import java.util.Map;
import java.util.Optional;
import java.util.function.Supplier;

public class R21Fz9ReviewEmUser1 {
    static final String H = "R21Fz9ReviewEmUser1";
    static final Map<String, byte[]> BYTES = new HashMap<>();
    static int goneAsks;
    static int goneSuperAsks;
    static boolean allowGone;

    static final class L extends ClassLoader {
        L() {
            super("fz9em", ClassLoader.getPlatformClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.equals("Fz9EmGone")) {
                goneAsks++;
                if (!allowGone) {
                    throw new ClassNotFoundException(name);
                }
                byte[] b = cls(name, null);
                return defineClass(name, b, 0, b.length);
            }
            if (name.equals("Fz9EmGoneSuper")) {
                goneSuperAsks++;
                throw new ClassNotFoundException(name);
            }
            byte[] b = BYTES.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    static ClassElement row(String inner, String name) {
        return InnerClassesAttribute.of(InnerClassInfo.of(ClassDesc.of(inner),
                Optional.empty(), Optional.ofNullable(name), 0));
    }

    static ClassElement em(String cls) {
        return EnclosingMethodAttribute.of(ClassDesc.of(cls), Optional.empty(), Optional.empty());
    }

    static byte[] cls(String name, String superName, ClassElement... attrs) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            if (superName != null) {
                cb.withSuperclass(ClassDesc.of(superName));
            }
            for (ClassElement a : attrs) {
                cb.with(a);
            }
        });
    }

    static String q(String s) {
        return s == null ? "null" : "\"" + s.replaceAll("/0x[0-9a-fA-F]+", "/0x*") + "\"";
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (NoClassDefFoundError e) {
            Throwable c = e.getCause();
            return "NCDFE(" + e.getMessage() + "<-"
                    + (c == null ? "none" : c.getClass().getName() + ": " + c.getMessage()) + ")";
        } catch (Throwable t) {
            return "EXC " + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        BYTES.put("Fz9EmHost", cls("Fz9EmHost", null));
        BYTES.put("Fz9EmBad", cls("Fz9EmBad", "Fz9EmGoneSuper"));
        BYTES.put("Fz9EmHost$L", cls("Fz9EmHost$L", null, row("Fz9EmHost$L", "L"), em("Fz9EmGone")));
        BYTES.put("Fz9EmHost$B", cls("Fz9EmHost$B", null, row("Fz9EmHost$B", "B"), em("Fz9EmBad")));
        BYTES.put("Fz9EmHost$C", cls("Fz9EmHost$C", null, row("Fz9EmHost$C", "C"), em("Fz9EmHost")));
        L l = new L();
        Class<?> lc = Class.forName("Fz9EmHost$L", false, l);
        Class<?> bc = Class.forName("Fz9EmHost$B", false, l);
        Class<?> cc = Class.forName("Fz9EmHost$C", false, l);

        System.out.println("$L simple=" + safe(() -> q(lc.getSimpleName())));
        System.out.println("$L canonical=" + safe(() -> q(lc.getCanonicalName())));
        System.out.println("$L local=" + safe(() -> Boolean.toString(lc.isLocalClass())));
        Class<?> arr = Array.newInstance(lc, 0).getClass();
        System.out.println("$L[] simple=" + safe(() -> q(arr.getSimpleName()))
                + " canonical=" + safe(() -> q(arr.getCanonicalName())));
        System.out.println("$B simple=" + safe(() -> q(bc.getSimpleName())));
        System.out.println("$B canonical=" + safe(() -> q(bc.getCanonicalName())));
        System.out.println("$B again=" + safe(() -> q(bc.getSimpleName())));
        System.out.println("$C simple=" + safe(() -> q(cc.getSimpleName()))
                + " canonical=" + safe(() -> q(cc.getCanonicalName()))
                + " local=" + safe(() -> Boolean.toString(cc.isLocalClass())));

        byte[] hb = cls(H + "$H", null, em(H + "Missing"));
        Class<?> hc = null;
        try {
            hc = MethodHandles.lookup().defineHiddenClass(hb, false).lookupClass();
        } catch (Throwable t) {
            System.out.println("$H define EXC " + t.getClass().getName());
        }
        if (hc != null) {
            Class<?> h = hc;
            System.out.println("$H simple=" + safe(() -> q(h.getSimpleName())));
            System.out.println("$H canonical=" + safe(() -> q(h.getCanonicalName())));
        }
        System.out.println("asks gone=" + goneAsks + " goneSuper=" + goneSuperAsks);

        allowGone = true;
        System.out.println("now loadable: " + safe(() -> { try { return Class.forName("Fz9EmGone", false, l).getName(); } catch (ClassNotFoundException e) { throw new RuntimeException(e); } })
                + " asks gone=" + goneAsks);
        System.out.println("$L after simple=" + safe(() -> q(lc.getSimpleName())));
        System.out.println("$L after canonical=" + safe(() -> q(lc.getCanonicalName())));
        System.out.println("asks gone=" + goneAsks + " goneSuper=" + goneSuperAsks);
    }
}
