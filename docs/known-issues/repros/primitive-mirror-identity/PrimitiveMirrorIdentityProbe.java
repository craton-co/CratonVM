import java.lang.annotation.Annotation;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Method;
import java.util.IdentityHashMap;
import java.util.Map;

/**
 * Is a primitive class mirror a singleton, whichever reflective path produced it?
 *
 * Spring's ClassUtils.resolvePrimitiveIfNecessary does
 * {@code primitiveWrapperTypeMap.get(clazz)} against an IdentityHashMap whose
 * keys are the {@code X.class} literals. If {@code Method.getReturnType()} ever
 * mints a *different* mirror for the same primitive, that lookup misses and the
 * method returns null — which is what the AOT sweep saw once, as
 * "Cannot invoke Class.isArray() because attributeType is null".
 */
public class PrimitiveMirrorIdentityProbe {

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.METHOD, ElementType.TYPE})
    public @interface Marker {
        boolean autowireCandidate() default true;
        int count() default 0;
        long big() default 0L;
        double ratio() default 0.0;
        char tag() default 'x';
        byte b() default 0;
        short s() default 0;
        float f() default 0.0f;
        String name() default "";
        Class<?> type() default Object.class;
    }

    @Marker
    static class Holder {
        public boolean flag() { return true; }
        public int n() { return 0; }
        public void v() {}
    }

    private static final Map<Class<?>, Class<?>> PRIMITIVE_TO_WRAPPER = new IdentityHashMap<>(9);
    static {
        PRIMITIVE_TO_WRAPPER.put(boolean.class, Boolean.class);
        PRIMITIVE_TO_WRAPPER.put(byte.class, Byte.class);
        PRIMITIVE_TO_WRAPPER.put(char.class, Character.class);
        PRIMITIVE_TO_WRAPPER.put(double.class, Double.class);
        PRIMITIVE_TO_WRAPPER.put(float.class, Float.class);
        PRIMITIVE_TO_WRAPPER.put(int.class, Integer.class);
        PRIMITIVE_TO_WRAPPER.put(long.class, Long.class);
        PRIMITIVE_TO_WRAPPER.put(short.class, Short.class);
        PRIMITIVE_TO_WRAPPER.put(void.class, Void.class);
    }

    private static int failures = 0;

    private static void check(String what, Class<?> got, Class<?> want) {
        if (got != want) {
            if (failures < 25) {
                System.out.println("  IDENTITY MISS " + what + " got=" + got
                        + "@" + System.identityHashCode(got)
                        + " want=" + want + "@" + System.identityHashCode(want));
            }
            failures++;
        }
        if (got != null && got.isPrimitive() && got != void.class
                && PRIMITIVE_TO_WRAPPER.get(got) == null) {
            if (failures < 25) {
                System.out.println("  MAP MISS      " + what + " " + got
                        + "@" + System.identityHashCode(got)
                        + " is primitive but absent from an IdentityHashMap keyed on the literals");
            }
            failures++;
        }
    }

    /** Every reflective route to a primitive mirror we can reach cheaply. */
    private static void round(int i) throws Exception {
        Class<?> ann = Marker.class;
        check("Marker.autowireCandidate().returnType", ann.getMethod("autowireCandidate").getReturnType(), boolean.class);
        check("Marker.count().returnType", ann.getMethod("count").getReturnType(), int.class);
        check("Marker.big().returnType", ann.getMethod("big").getReturnType(), long.class);
        check("Marker.ratio().returnType", ann.getMethod("ratio").getReturnType(), double.class);
        check("Marker.tag().returnType", ann.getMethod("tag").getReturnType(), char.class);
        check("Marker.b().returnType", ann.getMethod("b").getReturnType(), byte.class);
        check("Marker.s().returnType", ann.getMethod("s").getReturnType(), short.class);
        check("Marker.f().returnType", ann.getMethod("f").getReturnType(), float.class);

        check("Holder.flag().returnType", Holder.class.getMethod("flag").getReturnType(), boolean.class);
        check("Holder.n().returnType", Holder.class.getMethod("n").getReturnType(), int.class);
        check("Holder.v().returnType", Holder.class.getMethod("v").getReturnType(), void.class);

        check("Boolean.TYPE", Boolean.TYPE, boolean.class);
        check("Integer.TYPE", Integer.TYPE, int.class);
        check("Void.TYPE", Void.TYPE, void.class);
        check("boolean[].componentType", boolean[].class.getComponentType(), boolean.class);
        check("int[].componentType", int[].class.getComponentType(), int.class);
        check("Class.forName(boolean)", Boolean.TYPE, boolean.class);

        // The exact shape the AOT sweep hit: read the annotation off a class,
        // then resolve each attribute's declared type.
        Marker m = Holder.class.getAnnotation(Marker.class);
        for (Method attribute : m.annotationType().getDeclaredMethods()) {
            Class<?> rt = attribute.getReturnType();
            if (rt == null) {
                System.out.println("  NULL RETURN TYPE for " + attribute.getName() + " (round " + i + ")");
                failures++;
            }
            else if (rt.isPrimitive() && rt != void.class && PRIMITIVE_TO_WRAPPER.get(rt) == null) {
                System.out.println("  MAP MISS on attribute " + attribute.getName() + " (round " + i + ")");
                failures++;
            }
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        for (int i = 0; i < rounds; i++) {
            round(i);
            if ((i & 0x3FF) == 0x3FF) {
                System.gc();
            }
        }
        System.out.println(failures == 0
                ? "PROBE PASS (" + rounds + " rounds)"
                : "PROBE FAIL " + failures + " identity/map misses");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
