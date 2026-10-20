// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L3 --
// docs/internal/fixed-bugs/interpreter-i57-L3-proposal-compatible-declaration-annotations-run-the-jdks-parser-DONE-20261014.md:
// a declaration annotation whose `Class` member names a class its loader
// cannot find, and an annotation whose own type is missing. HotSpot's
// `AnnotationParser` keeps a `TypeNotPresentExceptionProxy`: the member
// access throws `TypeNotPresentException` naming the class, `toString`
// renders a warning, and the annotation of a missing type is skipped.
// `--compatible`'s annotation natives (CRATONVM_JDK_DECLARATION_ANNOTATIONS
// off) count the missing type's annotation (`count: 2`) and render the
// member as `java.lang.TypeNotPresentException` in `toString`.
// The classes (top-level, so no `InnerClasses` check pairs them with the
// application loader's outer class) are defined by a child-first loader that
// refuses `L3W58AmcGone` and `L3W58AmcGoneAnn`. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// count: 1
// value: java.lang.TypeNotPresentException Type L3W58AmcGone not present
// str: @L3W58AmcC(L3W58AmcGone.class /* Warning: type not present! */)
// method-value: java.lang.TypeNotPresentException Type L3W58AmcGone not present
// method-count: 1
// count-again: 1
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W58AnnotationMissingClass`.
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.annotation.Annotation;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.reflect.Method;

@Retention(RetentionPolicy.RUNTIME)
@interface L3W58AmcC {
    Class<?> value();
}

@Retention(RetentionPolicy.RUNTIME)
@interface L3W58AmcGoneAnn {
}

class L3W58AmcGone {
}

@L3W58AmcC(L3W58AmcGone.class)
@L3W58AmcGoneAnn
class L3W58AmcT {
    @L3W58AmcC(L3W58AmcGone.class)
    public void m() {
    }
}

public class L3W58AnnotationMissingClass {
    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (!name.startsWith("L3W58Amc")) {
                    return super.loadClass(name, resolve);
                }
                if (name.startsWith("L3W58AmcGone")) {
                    throw new ClassNotFoundException(name);
                }
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try (InputStream in = getParent().getResourceAsStream(name.replace('.', '/') + ".class")) {
                        ByteArrayOutputStream out = new ByteArrayOutputStream();
                        in.transferTo(out);
                        byte[] b = out.toByteArray();
                        c = defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        ClassLoader l = new ChildFirst(L3W58AnnotationMissingClass.class.getClassLoader());
        Class<?> t = Class.forName("L3W58AmcT", false, l);
        Class<? extends Annotation> c = (Class<? extends Annotation>) Class.forName("L3W58AmcC", false, l);
        Method value = c.getMethod("value");
        // `L3W58AmcC` is package-private in the child loader's runtime package.
        value.setAccessible(true);
        System.out.println("count: " + t.getAnnotations().length);
        Annotation onClass = t.getAnnotation(c);
        try {
            System.out.println("value: " + value.invoke(onClass));
        } catch (java.lang.reflect.InvocationTargetException e) {
            System.out.println("value: " + e.getCause().getClass().getName() + " " + e.getCause().getMessage());
        }
        System.out.println("str: " + onClass);
        Annotation onMethod = t.getMethod("m").getAnnotation(c);
        try {
            System.out.println("method-value: " + value.invoke(onMethod));
        } catch (java.lang.reflect.InvocationTargetException e) {
            System.out.println("method-value: " + e.getCause().getClass().getName() + " " + e.getCause().getMessage());
        }
        System.out.println("method-count: " + t.getMethod("m").getAnnotations().length);
        System.out.println("count-again: " + t.getAnnotations().length);
    }
}
