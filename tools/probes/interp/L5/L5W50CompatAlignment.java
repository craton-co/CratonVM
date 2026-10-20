// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 50, lane L5 -- the `--compatible` rows of the
// HotSpot-alignment table (`types/src/hotspot_alignment.rs`) and the JDK 25
// `AccessController` bodies, one row each, the fixed case first and again
// last on fresh classes (a first-call-only gap shows as a differing row):
//
//   component   `Class.forName("[Lp.C;", false, app).getComponentType()` runs
//               no initializer (row `component-type-from-array-class`;
//               `--compatible` before: `[C]`, the by-name route was
//               `ensure_class_initialized`).
//   find-slash  a user loader's `findLoadedClass("p/X")` of a class it defined
//               is null (`ClassLoader.checkName`; row
//               `find-loaded-name-check`; `--compatible` before: the class).
//   final-super a class extending a final class fails to define with HotSpot's
//               `IncompatibleClassChangeError` (row `supertype-shape-checks`;
//               `--compatible` before: defined).
//   module      a user loader's own class under a JDK package name is in that
//               loader's unnamed module (row `user-loader-unnamed-module`;
//               `--compatible` before: named `java.base`).
//   ac-context  `AccessController.getContext()` is one object, and its
//               `checkPermission` throws JEP 486's `AccessControlException`
//               (`CRATONVM_ACCESS_CONTROLLER_JDK_BODIES`; both modes before:
//               a fresh context per call, and allowed).
//
// Run (no setup):
//   javac -d out L5W50CompatAlignment.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W50CompatAlignment
//
// Positive control: `CRATONVM_COMPAT_ALIGN=none` under `--compatible` puts the
// first four rows back on the old answers (and
// `CRATONVM_ACCESS_CONTROLLER_JDK_BODIES=0` the last), in both passes.
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   component-1=L5W50CompatAlignment$C1 []
//   find-slash-1=true null
//   final-super-1=java.lang.IncompatibleClassChangeError: class p50.A cannot inherit from final class p50.F
//   module-1=false true
//   ac-context-1=true java.security.AccessControlException: checking permissions is not supported
//   component-2=L5W50CompatAlignment$C2 []
//   find-slash-2=true null
//   final-super-2=java.lang.IncompatibleClassChangeError: class p50.A cannot inherit from final class p50.F
//   module-2=false true
//   ac-context-2=true java.security.AccessControlException: checking permissions is not supported

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.security.AccessControlContext;
import java.security.AccessController;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.PropertyPermission;

@SuppressWarnings("removal")
public class L5W50CompatAlignment {
    static final List<String> INITS = new ArrayList<>();

    public static class C1 {
        static {
            INITS.add("C1");
        }
    }

    public static class C2 {
        static {
            INITS.add("C2");
        }
    }

    static final ClassDesc OBJECT = ClassDesc.of("java.lang.Object");

    static byte[] cls(String name, String sup, int flags) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(flags);
            cb.withSuperclass(sup == null ? OBJECT : ClassDesc.of(sup));
        });
    }

    /** Defines what it is given, parent-first for everything else. */
    static final class Gen extends ClassLoader {
        final Map<String, byte[]> classes = new HashMap<>();

        Gen(Map<String, byte[]> classes) {
            super(L5W50CompatAlignment.class.getClassLoader());
            this.classes.putAll(classes);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null && classes.containsKey(name)) {
                    c = findClass(name);
                }
                return c != null ? c : super.loadClass(name, resolve);
            }
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = classes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }

        Class<?> findLoaded(String name) {
            return findLoadedClass(name);
        }
    }

    static final int PUBLIC = 0x0001;
    static final int FINAL = 0x0010;
    static final int SUPER = 0x0020;

    static String component(String nested) throws Exception {
        INITS.clear();
        Class<?> a = Class.forName("[L" + L5W50CompatAlignment.class.getName() + "$" + nested + ";", false,
                L5W50CompatAlignment.class.getClassLoader());
        return a.getComponentType().getName() + " " + INITS;
    }

    static String findSlash(String name) throws Exception {
        Gen gen = new Gen(Map.of(name, cls(name, null, PUBLIC | SUPER)));
        Class<?> own = gen.loadClass(name);
        return (gen.findLoaded(name) == own) + " " + gen.findLoaded(name.replace('.', '/'));
    }

    static String finalSuper() {
        Gen gen = new Gen(Map.of(
                "p50.F", cls("p50.F", null, PUBLIC | SUPER | FINAL),
                "p50.A", cls("p50.A", "p50.F", PUBLIC | SUPER)));
        try {
            return "defined " + gen.loadClass("p50.A").getName();
        } catch (Throwable t) {
            return t.toString();
        }
    }

    static String module(String name) throws Exception {
        Gen gen = new Gen(Map.of(name, cls(name, null, PUBLIC | SUPER)));
        Class<?> own = gen.loadClass(name);
        return own.getModule().isNamed() + " " + (own.getModule() == gen.getUnnamedModule());
    }

    static String acContext() {
        AccessControlContext a = AccessController.getContext();
        AccessControlContext b = AccessController.getContext();
        String check;
        try {
            a.checkPermission(new PropertyPermission("user.home", "read"));
            check = "allowed";
        } catch (Throwable t) {
            check = t.toString();
        }
        return (a == b) + " " + check;
    }

    static void row(String label, java.util.concurrent.Callable<String> body) {
        String out;
        try {
            out = body.call();
        } catch (Throwable t) {
            out = "threw " + t;
        }
        System.out.println(label + "=" + out);
    }

    public static void main(String[] args) {
        row("component-1", () -> component("C1"));
        row("find-slash-1", () -> findSlash("q50.X1"));
        row("final-super-1", L5W50CompatAlignment::finalSuper);
        row("module-1", () -> module("javax.security.auth.x500.L5W50Own1"));
        row("ac-context-1", L5W50CompatAlignment::acContext);
        row("component-2", () -> component("C2"));
        row("find-slash-2", () -> findSlash("q50.X2"));
        row("final-super-2", L5W50CompatAlignment::finalSuper);
        row("module-2", () -> module("javax.security.auth.x500.L5W50Own2"));
        row("ac-context-2", L5W50CompatAlignment::acContext);
    }
}
