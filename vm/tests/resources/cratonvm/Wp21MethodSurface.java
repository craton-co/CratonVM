package cratonvm;

import java.io.IOException;
import java.lang.annotation.Annotation;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Method;

/**
 * WP2.1 — end-to-end probe of the {@code java.lang.reflect.Method} surface
 * (excluding {@code invoke}, which is WP2.2's lane).
 *
 * <p>Each public static method below maps to one Rust integration probe in
 * {@code vm/tests/wp2_1_method_surface.rs}. They follow the
 * {@code WpN_M*} convention: return a small int the Rust harness asserts
 * on (1 = pass, 0 = fail, negative = specific failure mode).
 *
 * <p>The surface tested:
 * <ul>
 *   <li>{@code getName}, {@code toString}, {@code getReturnType},
 *       {@code getParameterTypes}, {@code getExceptionTypes},
 *       {@code getModifiers}, {@code getDeclaringClass}</li>
 *   <li>{@code isDefault} (interface default vs. class method),
 *       {@code isVarArgs}, {@code isSynthetic}, {@code isBridge}</li>
 *   <li>{@code getParameterAnnotations}, {@code getDefaultValue},
 *       {@code getAnnotation(Class)}, {@code getAnnotations}</li>
 *   <li>{@code getGenericReturnType}, {@code getGenericParameterTypes},
 *       {@code getGenericExceptionTypes}</li>
 * </ul>
 */
public class Wp21MethodSurface {

    /** Marker annotation on a method. */
    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.METHOD)
    public @interface Marker {
        String value() default "default-marker";
    }

    /** Annotation with default value — for getDefaultValue(). */
    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.METHOD)
    public @interface WithDefault {
        int level() default 42;
    }

    /** Parameter-level annotation — for getParameterAnnotations(). */
    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.PARAMETER)
    public @interface ParamMark {
    }

    /** Concrete method that throws checked exceptions. */
    @Marker("on-foo")
    public int foo(String s, int n) throws IOException {
        return s.length() + n;
    }

    /** Varargs method — exercises isVarArgs / Object[] descriptor. */
    public Object[] varargsMethod(Object... args) {
        return args;
    }

    /** Method with parameter annotation. */
    public void paramAnnotated(@ParamMark String tagged, int plain) { }

    /**
     * Generic return type (List<String>) for getGenericReturnType().
     */
    public java.util.List<String> genericReturn() {
        return java.util.Collections.emptyList();
    }

    /**
     * Interface that declares a default method — for isDefault() on
     * the interface side.
     */
    public interface WithDefaultMethod {
        default String greet() {
            return "hi";
        }
        String abstractGreet();
    }

    /**
     * Generic-parameterized base — combined with ConcreteImpl below to
     * force javac to emit a synthetic bridge method, exercising
     * isBridge() / isSynthetic().
     */
    public static class GenericBase<T> {
        public T identity(T t) { return t; }
    }

    public static class ConcreteImpl extends GenericBase<String> {
        @Override
        public String identity(String s) { return s; }
    }

    // -------------------- probes --------------------

    /**
     * Returns 1 iff getName / toString / getReturnType / getParameterTypes /
     * getExceptionTypes / getModifiers / getDeclaringClass all behave
     * correctly on `foo(String, int) throws IOException`.
     *
     * Negative sentinels:
     *   -1 = exception escaped
     *   -2 = getDeclaredMethod returned null
     *   -3 = name mismatch
     *   -4 = toString null or missing "foo"
     *   -5 = returnType != int.class
     *   -6 = parameterTypes wrong shape
     *   -7 = exceptionTypes missing IOException
     *   -8 = modifiers wrong
     *   -9 = declaringClass wrong
     */
    public static int basicSurface() {
        try {
            Method m = Wp21MethodSurface.class.getDeclaredMethod("foo", String.class, int.class);
            if (m == null) return -2;
            if (!"foo".equals(m.getName())) return -3;
            String s = m.toString();
            if (s == null || !s.contains("foo")) return -4;
            if (m.getReturnType() != int.class) return -5;
            Class<?>[] pts = m.getParameterTypes();
            if (pts == null || pts.length != 2 || pts[0] != String.class || pts[1] != int.class) return -6;
            Class<?>[] ets = m.getExceptionTypes();
            if (ets == null || ets.length != 1 || ets[0] != IOException.class) return -7;
            int mods = m.getModifiers();
            // public, not static
            if ((mods & java.lang.reflect.Modifier.PUBLIC) == 0) return -8;
            if ((mods & java.lang.reflect.Modifier.STATIC) != 0) return -8;
            if (m.getDeclaringClass() != Wp21MethodSurface.class) return -9;
            return 1;
        } catch (Throwable t) {
            return -1;
        }
    }

    /**
     * Returns 1 iff:
     *   - WithDefaultMethod.greet() reports isDefault() == true
     *   - WithDefaultMethod.abstractGreet() reports isDefault() == false
     *   - basicSurface's foo() reports isDefault() == false
     *   - varargsMethod() reports isVarArgs() == true
     *   - foo() reports isVarArgs() == false
     */
    public static int booleanFlags() {
        try {
            Method greet = WithDefaultMethod.class.getDeclaredMethod("greet");
            if (!greet.isDefault()) return 0;
            Method absGreet = WithDefaultMethod.class.getDeclaredMethod("abstractGreet");
            if (absGreet.isDefault()) return 0;
            Method foo = Wp21MethodSurface.class.getDeclaredMethod("foo", String.class, int.class);
            if (foo.isDefault()) return 0;
            Method va = Wp21MethodSurface.class.getDeclaredMethod("varargsMethod", Object[].class);
            if (!va.isVarArgs()) return 0;
            if (foo.isVarArgs()) return 0;
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * Returns 1 iff a generic-erasure bridge method exists on
     * ConcreteImpl ({@code Object identity(Object)}) and reports
     * isBridge() == true AND isSynthetic() == true; AND the user-visible
     * {@code String identity(String)} reports both flags as false.
     */
    public static int bridgeAndSynthetic() {
        try {
            Method[] all = ConcreteImpl.class.getDeclaredMethods();
            boolean foundBridge = false;
            boolean foundUser = false;
            for (Method m : all) {
                if (!"identity".equals(m.getName())) continue;
                Class<?>[] pts = m.getParameterTypes();
                if (pts.length != 1) continue;
                if (pts[0] == Object.class) {
                    if (m.isBridge() && m.isSynthetic()) {
                        foundBridge = true;
                    }
                } else if (pts[0] == String.class) {
                    if (!m.isBridge() && !m.isSynthetic()) {
                        foundUser = true;
                    }
                }
            }
            return (foundBridge && foundUser) ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * Returns 1 iff:
     *   - foo's @Marker annotation is reachable via getAnnotation(Marker.class)
     *   - foo's getAnnotations() includes the Marker
     *   - paramAnnotated's getParameterAnnotations() shape is [{@code [@ParamMark]}, []]
     */
    public static int annotationsAndParams() {
        try {
            Method foo = Wp21MethodSurface.class.getDeclaredMethod("foo", String.class, int.class);
            Marker mk = foo.getAnnotation(Marker.class);
            if (mk == null) return 0;
            Annotation[] anns = foo.getAnnotations();
            if (anns == null) return 0;
            boolean foundMarker = false;
            for (Annotation a : anns) {
                if (a instanceof Marker) { foundMarker = true; break; }
            }
            if (!foundMarker) return 0;

            Method pa = Wp21MethodSurface.class.getDeclaredMethod("paramAnnotated", String.class, int.class);
            Annotation[][] pann = pa.getParameterAnnotations();
            if (pann == null) return 0;
            if (pann.length != 2) return 0;
            // First param has @ParamMark
            boolean p0 = false;
            for (Annotation a : pann[0]) {
                if (a instanceof ParamMark) { p0 = true; break; }
            }
            if (!p0) return 0;
            // Second param has none
            if (pann[1] == null) return 0;
            for (Annotation a : pann[1]) {
                if (a instanceof ParamMark) return 0;
            }
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * Returns the int value of WithDefault.level()'s default value
     * surfaced via Method.getDefaultValue(). Expected: 42. On any failure
     * (incl. missing method), returns 0.
     *
     * NB: This probes the annotation-method default-value surface; the
     * result is the default-element value, NOT the int 1 (so the Rust
     * test asserts against 42 specifically).
     */
    public static int annotationDefaultValue() {
        try {
            Method levelMethod = WithDefault.class.getDeclaredMethod("level");
            Object dv = levelMethod.getDefaultValue();
            if (dv instanceof Integer) {
                return ((Integer) dv).intValue();
            }
            return 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * Returns 1 iff:
     *   - genericReturn().getGenericReturnType() is non-null
     *   - genericReturn().getGenericParameterTypes() is non-null and zero-length
     *   - foo().getGenericExceptionTypes() is non-null and length 1
     */
    public static int genericTypes() {
        try {
            Method gr = Wp21MethodSurface.class.getDeclaredMethod("genericReturn");
            java.lang.reflect.Type gret = gr.getGenericReturnType();
            if (gret == null) return 0;
            java.lang.reflect.Type[] gpts = gr.getGenericParameterTypes();
            if (gpts == null || gpts.length != 0) return 0;
            Method foo = Wp21MethodSurface.class.getDeclaredMethod("foo", String.class, int.class);
            java.lang.reflect.Type[] gets = foo.getGenericExceptionTypes();
            if (gets == null || gets.length != 1) return 0;
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * Closure probe — runs every other probe and returns 1 iff all return
     * their expected values.
     */
    public static int closesMethodSurface() {
        if (basicSurface() != 1) return 0;
        if (booleanFlags() != 1) return 0;
        if (bridgeAndSynthetic() != 1) return 0;
        if (annotationsAndParams() != 1) return 0;
        if (annotationDefaultValue() != 42) return 0;
        if (genericTypes() != 1) return 0;
        return 1;
    }
}
