// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.List;

/**
 * Two classloaders, one binary name: what a class defined by a FORKED loader sees,
 * against the application's own copy. Models Spring's
 * {@code CompileWithForkedClassLoaderClassLoader} (parent = the platform loader,
 * {@code findClass} defines the application's classes from the application
 * loader's bytes, {@code loadClass(String)} overridden to call {@code super}).
 * See docs/internal/fixed-suite-bugs/spring/
 * spring-jdkonly-forked-loader-assignability-FIXED-20260919.md.
 *
 * <p>Every row is deterministic; run under HotSpot and under {@code --jdk-only} and
 * every line must match.
 *
 * <pre>
 *   java SpringJdkOnlyForkedLoaderProbe
 *   cratonvm --jdk-only --java-home $JAVA_HOME -cp . SpringJdkOnlyForkedLoaderProbe
 * </pre>
 */
public class SpringJdkOnlyForkedLoaderProbe {

    // ---- fixtures: every one of these is defined twice (application + fork) ----

    public interface Bundle {
        String hi();
    }

    public static class BundleImpl implements Bundle {
        public static final BundleImpl INSTANCE = new BundleImpl();

        public String hi() {
            return "hi";
        }
    }

    public interface Late {
    }

    public static class LateImpl implements Late {
    }

    /** Never touched by the application before the fork defines its own copy. */
    public static class Cold implements Bundle {
        public static final Cold INSTANCE = new Cold();
        public static int inits;

        static {
            inits++;
        }

        public String hi() {
            return "cold";
        }
    }

    public static class Cold2 implements Bundle {
        public static final Cold2 INSTANCE = new Cold2();
        public static Object slot;
        public static int counter;
        public int inst;

        public static Cold2 make() {
            return new Cold2();
        }

        public Class<?> me() {
            return getClass();
        }

        public String hi() {
            return "cold2";
        }
    }

    public enum Mode {
        A, B
    }

    @Retention(RetentionPolicy.RUNTIME)
    public @interface Anno {
        Mode mode() default Mode.A;

        Mode[] modes() default {Mode.B};
    }

    @Anno(mode = Mode.B)
    public static class Target {
        public Mode field;

        public Mode read(Mode in) {
            return in;
        }
    }

    static final class Fork extends ClassLoader {
        final ClassLoader app;

        Fork(ClassLoader app) {
            super(app.getParent());
            this.app = app;
        }

        @Override
        public Class<?> loadClass(String name) throws ClassNotFoundException {
            return super.loadClass(name);
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            String res = name.replace('.', '/') + ".class";
            try (InputStream in = app.getResourceAsStream(res)) {
                if (in == null) {
                    throw new ClassNotFoundException(name);
                }
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length, null);
            } catch (java.io.IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    static void row(String n, Object v) {
        System.out.println(n + " = " + v);
    }

    static String who(Class<?> c, ClassLoader fork) {
        ClassLoader l = c.getClassLoader();
        return l == fork ? "fork" : l == SpringJdkOnlyForkedLoaderProbe.class.getClassLoader() ? "app" : String.valueOf(l);
    }

    static boolean casts(Class<?> target, Object o) {
        try {
            target.cast(o);
            return true;
        } catch (ClassCastException e) {
            return false;
        }
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    public static void main(String[] args) throws Throwable {
        ClassLoader app = SpringJdkOnlyForkedLoaderProbe.class.getClassLoader();

        // 1. Reflective assignability, cast and asSubclass across the two namespaces (bytecode
        // instanceof / checkcast keep a name rule of their own; see the known-issues page).
        // The application has ALREADY loaded and initialized its BundleImpl.
        Class<?> appImpl = Class.forName(BundleImpl.class.getName(), true, app);
        Fork fork = new Fork(app);
        Class<?> forkIface = fork.loadClass(Bundle.class.getName());
        row("fork.loadClass(Bundle) defined by", who(forkIface, fork));
        row("fork Bundle != app Bundle", forkIface != Bundle.class);
        // jboss-logging: Class.forName(bundle, true, Messages.class.getClassLoader()).asSubclass(Messages)
        Class<?> forkImpl = Class.forName(BundleImpl.class.getName(), true, forkIface.getClassLoader());
        row("forName(impl, true, fork) defined by", who(forkImpl, fork));
        row("forkImpl != appImpl", forkImpl != appImpl);
        row("forkIface.isAssignableFrom(forkImpl)", forkIface.isAssignableFrom(forkImpl));
        row("app Bundle.isAssignableFrom(forkImpl)", Bundle.class.isAssignableFrom(forkImpl));
        row("forkIface.isAssignableFrom(appImpl)", forkIface.isAssignableFrom(appImpl));
        row("forkImpl.getInterfaces()[0] == forkIface", forkImpl.getInterfaces()[0] == forkIface);
        Object forkInstance = forkImpl.getDeclaredConstructor().newInstance();
        Object appInstance = appImpl.getDeclaredConstructor().newInstance();
        row("forkIface.cast(fork instance)", casts(forkIface, forkInstance));
        row("forkIface.cast(app instance)", casts(forkIface, appInstance));
        row("app Bundle.cast(fork instance)", casts(Bundle.class, forkInstance));
        row("forkImpl.asSubclass(app Bundle)", casts(Bundle.class, forkInstance) && Bundle.class.isAssignableFrom(forkImpl));
        row("forkImpl.asSubclass(forkIface)", forkImpl.asSubclass(forkIface) == forkImpl);
        row("forkIface.isInstance(fork INSTANCE)", forkIface.isInstance(forkImpl.getField("INSTANCE").get(null)));
        Class<?> forkLateIface = fork.loadClass(Late.class.getName());
        Class<?> forkLateImpl = Class.forName(LateImpl.class.getName(), true, fork);
        row("late: forName(impl, true, fork) defined by", who(forkLateImpl, fork));
        row("late: fork Late.isAssignableFrom(forkLateImpl)", forkLateIface.isAssignableFrom(forkLateImpl));
        row("late: app Late.isAssignableFrom(forkLateImpl)", Late.class.isAssignableFrom(forkLateImpl));
        Class<?> appLate = Class.forName(LateImpl.class.getName(), true, app);
        row("late: app copy != fork copy", appLate != forkLateImpl);
        row("late: forkLate.isAssignableFrom(appLate)", forkLateIface.isAssignableFrom(appLate));

        // 2. MethodHandles on a fork-defined class name THAT class, not the application's copy
        // (jboss-logging 3.6: lookup.findStaticGetter(impl, "INSTANCE", impl).invoke()).
        Class<?> forkCold = Class.forName(Cold.class.getName(), false, forkIface.getClassLoader());
        MethodHandles.Lookup lk = MethodHandles.privateLookupIn(forkCold, MethodHandles.lookup());
        Object cv = lk.findStaticGetter(forkCold, "INSTANCE", forkCold).invoke();
        row("mh findStaticGetter().invoke() is the fork's class", cv.getClass() == forkCold);
        row("mh findStaticGetter value isInstance(forkIface)", forkIface.isInstance(cv));
        row("mh fork copy initialised once", forkCold.getField("inits").get(null));
        row("mh app copy first touched now, inits", Cold.inits);
        Class<?> f2 = Class.forName(Cold2.class.getName(), false, forkIface.getClassLoader());
        MethodHandles.Lookup l2 = MethodHandles.privateLookupIn(f2, MethodHandles.lookup());
        MethodType none = MethodType.methodType(void.class);
        Object fi = l2.findConstructor(f2, none).invoke();
        row("mh findConstructor -> fork class", fi.getClass() == f2);
        row("mh findStatic make() -> fork class", l2.findStatic(f2, "make", MethodType.methodType(f2)).invoke().getClass() == f2);
        row("mh findVirtual me() -> fork class", l2.findVirtual(f2, "me", MethodType.methodType(Class.class)).invoke(fi) == f2);
        row("mh unreflectGetter(static) -> fork class", l2.unreflectGetter(f2.getField("INSTANCE")).invoke().getClass() == f2);
        Method make = f2.getMethod("make");
        row("mh unreflect(static Method) -> fork class", l2.unreflect(make).invoke().getClass() == f2);
        Object marker = new Object();
        l2.findStaticSetter(f2, "slot", Object.class).invoke(marker);
        row("mh findStaticSetter visible on fork copy", f2.getField("slot").get(null) == marker);
        row("mh findStaticSetter not visible on app copy", Cold2.slot != marker);
        VarHandle svh = l2.findStaticVarHandle(f2, "counter", int.class);
        svh.set(41);
        row("mh findStaticVarHandle on fork copy", f2.getField("counter").get(null));
        row("mh findStaticVarHandle on app copy", Cold2.counter);
        VarHandle ivh = l2.findVarHandle(f2, "inst", int.class);
        ivh.set(fi, 7);
        row("mh findVarHandle inst", f2.getField("inst").getInt(fi));
        row("mh findGetter inst", (int) l2.findGetter(f2, "inst", int.class).invoke(fi));

        // 3. The Method a Proxy hands its InvocationHandler carries the FORK's types.
        // (Spring's TypeMappedAnnotation.adapt checks the value against getReturnType().)
        Class<?> fMode = fork.loadClass(Mode.class.getName());
        Class<?> fAnno = fork.loadClass(Anno.class.getName());
        Class<?> fTarget = fork.loadClass(Target.class.getName());
        Method m = fAnno.getMethod("mode");
        row("Method.getReturnType() == fork Mode", m.getReturnType() == fMode);
        row("getMethods() mode return == fork Mode", returnOf(fAnno.getMethods(), "mode") == fMode);
        row("modes() return component == fork Mode", fAnno.getMethod("modes").getReturnType().getComponentType() == fMode);
        row("getDefaultValue().getClass() == fork Mode", m.getDefaultValue().getClass() == fMode);
        Object annotation = fTarget.getAnnotation(fAnno.asSubclass(java.lang.annotation.Annotation.class));
        Object val = m.invoke(annotation);
        row("annotation.mode().getClass() == fork Mode", val.getClass() == fMode);
        row("fork Mode.isInstance(annotation value)", fMode.isInstance(val));
        row("app Mode.isInstance(annotation value)", Mode.class.isInstance(val));
        Method read = fTarget.getMethod("read", fMode);
        row("read(Mode) return/param == fork Mode", read.getReturnType() == fMode && read.getParameterTypes()[0] == fMode);
        row("field type == fork Mode", fTarget.getField("field").getType() == fMode);
        List<String> seen = new ArrayList<>();
        InvocationHandler h = (proxy, method, a) -> {
            if (method.getName().equals("mode")) {
                seen.add("declaring == fork Anno: " + (method.getDeclaringClass() == fAnno));
                seen.add("returnType == fork Mode: " + (method.getReturnType() == fMode));
                seen.add("returnType == app Mode: " + (method.getReturnType() == Mode.class));
                return fMode.getEnumConstants()[1];
            }
            return null;
        };
        Object px = Proxy.newProxyInstance(fork, new Class<?>[] {fAnno}, h);
        Object pv = fAnno.getMethod("mode").invoke(px);
        for (String s : seen) {
            row("proxy handler Method: " + s.substring(0, s.indexOf(':')), s.substring(s.indexOf(':') + 2));
        }
        row("proxy value is the fork's Mode", pv.getClass() == fMode);
        row("fork Mode.isInstance(proxy value)", fMode.isInstance(pv));
        row("fork Anno.isInstance(proxy)", fAnno.isInstance(px));
        row("app Anno.isInstance(proxy)", Anno.class.isInstance(px));
        System.exit(0);
    }

    static Class<?> returnOf(Method[] methods, String name) {
        for (Method x : methods) {
            if (x.getName().equals(name)) {
                return x.getReturnType();
            }
        }
        return null;
    }
}
