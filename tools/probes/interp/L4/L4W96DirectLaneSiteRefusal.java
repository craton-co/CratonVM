// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 96, lane L4: the site-access refusal of an
// `invokeExact` / `invoke` site served by a REAL `DirectMethodHandle` at its
// own type. `lang_invoke::mh_real_direct_fast` (SBR3 mhperf) never asked
// `mh_site_access_refusal`; it relied on its callers asking first, so whether
// the `IllegalAccessError` was raised depended on which lane served the call.
// The lane now asks it itself for a caller's site, as `mh_linked_invoker` does
// (and declines a site naming a class two loaders define, so the ordinary
// path's loader refusals answer in their order).
//
// HotSpot links an `invokehandle` site by resolving its `MethodType` through
// the caller (`SystemDictionary::find_method_handle_type` ->
// `find_java_mirror_for_type`, which for each named class emulates
// `ConstantPool::verify_constant_pool_resolve` with
// `LinkResolver::check_klass_accessibility`). Here the caller `Caller` is
// defined by the child-first loader "child", which delegates `Box` (package
// private) to the application loader: another runtime package, so the link
// fails with `IllegalAccessError` before the handle's type is compared or the
// member runs, on every call (a failed link is not cached). A second loader
// ("other") defines its own `Box` so the name has two definitions (the shape
// CratonVM's doors classify); it changes nothing on HotSpot.
//
// No arguments, no setup.
// Run: javac -d . L4W96DirectLaneSiteRefusal.java
//      java -cp . L4W96DirectLaneSiteRefusal
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W96DirectLaneSiteRefusal
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from the sources above; the
// loader clauses' wording follows `Reflection::verify_class_access_msg` and is
// not certain to the character)
// child-exact-1: java.lang.IllegalAccessError: failed to access class L4W96DirectLaneSiteRefusal$Box from class L4W96DirectLaneSiteRefusal$Caller (L4W96DirectLaneSiteRefusal$Box is in unnamed module of loader 'app'; L4W96DirectLaneSiteRefusal$Caller is in unnamed module of loader 'child' @<hash>)
// child-exact-2: java.lang.IllegalAccessError: failed to access class L4W96DirectLaneSiteRefusal$Box from class L4W96DirectLaneSiteRefusal$Caller (L4W96DirectLaneSiteRefusal$Box is in unnamed module of loader 'app'; L4W96DirectLaneSiteRefusal$Caller is in unnamed module of loader 'child' @<hash>)
// child-invoke: java.lang.IllegalAccessError: failed to access class L4W96DirectLaneSiteRefusal$Box from class L4W96DirectLaneSiteRefusal$Caller (L4W96DirectLaneSiteRefusal$Box is in unnamed module of loader 'app'; L4W96DirectLaneSiteRefusal$Caller is in unnamed module of loader 'child' @<hash>)
// app-exact: 7
// app-invoke: 7
// HOTSPOT_EXPECTED_END
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W96DirectLaneSiteRefusal {
    static class Box {
    }

    public static class Target {
        public static int get(Box b) {
            return b == null ? 7 : 8;
        }
    }

    public static class Caller {
        // No branch, no handler: no stack map frame names `Box`.
        static int exact0(MethodHandle mh) throws Throwable {
            Box b = null;
            return (int) mh.invokeExact(b);
        }

        static int invoke0(MethodHandle mh) throws Throwable {
            Box b = null;
            return (int) mh.invoke(b);
        }

        public static int exact(MethodHandle mh) throws Throwable {
            return exact0(mh);
        }

        public static int invoke(MethodHandle mh) throws Throwable {
            return invoke0(mh);
        }
    }

    /** Defines exactly the one nested class `own` itself; delegates the rest. */
    static final class OneOwn extends ClassLoader {
        private final String own;

        OneOwn(String name, String own) {
            super(name, L4W96DirectLaneSiteRefusal.class.getClassLoader());
            this.own = own;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (!name.equals(own)) {
                    return super.loadClass(name, resolve);
                }
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                try (InputStream in = getParent().getResourceAsStream(name.replace('.', '/') + ".class")) {
                    byte[] b = in.readAllBytes();
                    return defineClass(name, b, 0, b.length);
                } catch (Exception e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            out = t.getClass().getName() + ": " + t.getMessage();
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out.replaceAll("@[0-9a-f]+", "@<hash>"));
    }

    public static void main(String[] args) throws Throwable {
        // A second definition of `Box`.
        new OneOwn("other", "L4W96DirectLaneSiteRefusal$Box").loadClass("L4W96DirectLaneSiteRefusal$Box");
        MethodHandle mh = MethodHandles.lookup()
                .findStatic(Target.class, "get", MethodType.methodType(int.class, Box.class));
        Class<?> caller = new OneOwn("child", "L4W96DirectLaneSiteRefusal$Caller")
                .loadClass("L4W96DirectLaneSiteRefusal$Caller");
        Method exact = caller.getMethod("exact", MethodHandle.class);
        Method invoke = caller.getMethod("invoke", MethodHandle.class);
        row("child-exact-1", () -> exact.invoke(null, mh));
        row("child-exact-2", () -> exact.invoke(null, mh));
        row("child-invoke", () -> invoke.invoke(null, mh));
        row("app-exact", () -> Caller.exact(mh));
        row("app-invoke", () -> Caller.invoke(mh));
    }
}
