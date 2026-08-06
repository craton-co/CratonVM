// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Is every annotation this VM hands back actually a `java.lang.annotation.Annotation`?
//
// JLS 9.6: every annotation interface implicitly extends
// `java.lang.annotation.Annotation`, so a `Proxy` built over one implements it
// transitively and `(Annotation) proxy` always succeeds. ByteBuddy relies on
// exactly that — `AnnotationList$ForLoadedAnnotations.get` casts each element of
// `getDeclaredAnnotations()` to `Annotation` — so a proxy that fails the cast
// takes out annotation reading for every consumer, and on this VM it surfaced as
// Mockito refusing to mock a class:
//
//   ClassCastException: jdk.proxy1.$Proxy42 cannot be cast to
//     java.lang.annotation.Annotation
//     at net.bytebuddy...AnnotationList$ForLoadedAnnotations.get(...)
//   -> IllegalStateException at InlineBytecodeGenerator.triggerRetransformation
//   -> MockitoException: Could not modify all classes [...]
//
// For each named class this walks the class, its methods, its fields and its
// constructors, and reports every annotation whose runtime object is not an
// `Annotation`. Reports the proxy's interface list too, since "which interfaces
// did the proxy actually get" is the question a failure raises next.
//
// Usage: AnnotationProxyIsAnnotationProbe <class-name>...

import java.lang.annotation.Annotation;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;

public class AnnotationProxyIsAnnotationProbe {

    static int checked = 0;
    static int bad = 0;

    static void check(String where, Object a) {
        checked++;
        if (a instanceof Annotation) {
            // Positive control for any "is this really an annotation type?"
            // guard: print the SAME two properties for the annotations that DO
            // work, so a guard written against them can be seen to keep them
            // rather than assumed to.
            Class<?> t = ((Annotation) a).annotationType();
            System.out.println("ok   " + where + " -> " + t.getName()
                    + "  isInterface=" + t.isInterface()
                    + "  isAnnotation=" + t.isAnnotation()
                    + "  superinterfaces=" + names(t.getInterfaces()));
            return;
        }
        bad++;
        Class<?> c = a == null ? null : a.getClass();
        System.out.println("BAD  " + where + " -> " + (c == null ? "null" : c.getName()));
        if (c != null) {
            System.out.println("       isProxyClass=" + Proxy.isProxyClass(c));
            for (Class<?> i : c.getInterfaces()) {
                // The annotation type the proxy was built over. `isAnnotation()`
                // and the superinterface list are the two things that decide
                // whether the proxy can possibly be an `Annotation`, and a
                // fabricated stand-in for a type that is not on the classpath
                // has neither.
                System.out.println("       implements " + i.getName()
                        + "  isInterface=" + i.isInterface()
                        + "  isAnnotation=" + i.isAnnotation()
                        + "  superinterfaces=" + names(i.getInterfaces()));
            }
        }
    }

    static String names(Class<?>[] cs) {
        if (cs.length == 0) {
            return "[]";
        }
        StringBuilder b = new StringBuilder("[");
        for (int i = 0; i < cs.length; i++) {
            if (i > 0) {
                b.append(", ");
            }
            b.append(cs[i].getName());
        }
        return b.append(']').toString();
    }

    /// `getDeclaredAnnotations()` returns `Annotation[]`, so a bad element can
    /// blow up on the array store or the loop variable rather than reaching
    /// `check`. Read it as `Object[]` where possible and report the throw
    /// itself as the finding.
    static void scan(String where, java.lang.reflect.AnnotatedElement e) {
        Object[] anns;
        try {
            anns = e.getDeclaredAnnotations();
        } catch (Throwable t) {
            bad++;
            System.out.println("BAD  " + where + " -> getDeclaredAnnotations() threw " + t);
            return;
        }
        for (Object a : anns) {
            check(where, a);
        }
    }

    public static void main(String[] args) {
        for (String name : args) {
            Class<?> cls;
            try {
                cls = Class.forName(name);
            } catch (Throwable t) {
                System.out.println("SKIP " + name + " (" + t.getClass().getSimpleName() + ")");
                continue;
            }
            scan(name, cls);
            for (Method m : cls.getDeclaredMethods()) {
                scan(name + "." + m.getName() + "()", m);
            }
            for (Field f : cls.getDeclaredFields()) {
                scan(name + "#" + f.getName(), f);
            }
            for (Constructor<?> c : cls.getDeclaredConstructors()) {
                scan(name + ".<init>", c);
            }
            for (Class<?> i : cls.getInterfaces()) {
                scan(i.getName() + " (interface of " + name + ")", i);
            }
        }
        System.out.println("SUMMARY checked=" + checked + " bad=" + bad);
        System.out.println(bad == 0 ? "PROBE-OK" : "PROBE-FAIL " + bad);
        if (bad != 0) {
            System.exit(1);
        }
    }
}
