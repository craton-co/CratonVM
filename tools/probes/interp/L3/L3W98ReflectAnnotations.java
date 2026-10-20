import java.lang.annotation.*;
import java.lang.reflect.*;
import java.util.*;

/**
 * Interpreter round i4 wave 98, lane L3: annotations as HotSpot 25 reports
 * them -- declared vs inherited (@Inherited), repeatable containers
 * (getAnnotationsByType vs getAnnotations), annotation toString / equals /
 * hashCode / defaults, parameter annotations (including an inner class
 * constructor's implicit outer parameter), type annotations, package-less
 * RUNTIME vs CLASS retention, annotations on records' components, fields and
 * accessors, and an annotation's own meta-annotations.
 */
public class L3W98ReflectAnnotations {
    @Retention(RetentionPolicy.RUNTIME) @Inherited @interface Inh { String value(); }
    @Retention(RetentionPolicy.RUNTIME) @interface NotInh { int value() default 7; }
    @Retention(RetentionPolicy.CLASS) @interface ClassOnly {}
    @Retention(RetentionPolicy.RUNTIME) @Repeatable(Tags.class) @interface Tag { String value(); }
    @Retention(RetentionPolicy.RUNTIME) @interface Tags { Tag[] value(); }
    @Retention(RetentionPolicy.RUNTIME) @interface Many {
        int i() default 1; long l() default 2L; char c() default 'c'; double d() default 1.5; float f() default 2.5f;
        byte b() default 3; short s() default 4; boolean z() default true; String str() default "q\"x\n";
        Class<?> cls() default int[].class; RetentionPolicy e() default RetentionPolicy.SOURCE;
        int[] arr() default {1, 2}; String[] strs() default {}; Tag tag() default @Tag("d");
    }
    @Retention(RetentionPolicy.RUNTIME) @Target({ElementType.TYPE_USE, ElementType.TYPE_PARAMETER}) @interface TU { String value() default ""; }
    @Retention(RetentionPolicy.RUNTIME) @Target(ElementType.PARAMETER) @interface P { int value(); }
    @Retention(RetentionPolicy.RUNTIME) @interface RC { String value(); }

    @Inh("base") @NotInh @ClassOnly @Tag("one")
    static class Base {}

    @Tag("a") @Tag("b")
    static class Sub extends Base {}

    @Inh("over") @Tags({@Tag("x")}) @Tag("y")
    static class Sub2 extends Base {}

    @Many static class WithMany {}
    @Many(i = -1, l = Long.MIN_VALUE, c = '\'', d = Double.NaN, f = Float.POSITIVE_INFINITY, b = -128,
          s = Short.MAX_VALUE, z = false, str = "", cls = Map.Entry.class, e = RetentionPolicy.RUNTIME,
          arr = {}, strs = {"a", "b"}, tag = @Tag("t"))
    static class WithManyCustom {}

    class Inner {
        Inner(@P(1) int a, @P(2) String b) {}
    }

    enum En { X; En(@P(9) String unused) { } En() {} }

    record Rec(@RC("comp") @TU("tu") List<@TU("arg") String> names, @P(3) int n) {}

    static <@TU("tp") T extends @TU("bound") Comparable<T>> @TU("ret") List<@TU("elem") ? extends T> tu(
            @P(4) @TU("p") Map<@TU("k") String, int @TU("dim") []> m) throws @TU("ex") RuntimeException {
        return null;
    }

    static List<String> sorted(Annotation[] as) {
        List<String> out = new ArrayList<>();
        for (Annotation a : as) out.add(a.toString());
        Collections.sort(out);
        return out;
    }

    static String params(Annotation[][] pa) {
        StringBuilder sb = new StringBuilder("[");
        for (Annotation[] a : pa) sb.append(sorted(a));
        return sb.append("]").toString();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("Base annotations: " + sorted(Base.class.getAnnotations()));
        System.out.println("Sub annotations: " + sorted(Sub.class.getAnnotations()));
        System.out.println("Sub declared: " + sorted(Sub.class.getDeclaredAnnotations()));
        System.out.println("Sub Inh: " + Sub.class.getAnnotation(Inh.class));
        System.out.println("Sub declared Inh: " + Sub.class.getDeclaredAnnotation(Inh.class));
        System.out.println("Sub NotInh: " + Sub.class.getAnnotation(NotInh.class));
        System.out.println("Sub Tag direct: " + Sub.class.getAnnotation(Tag.class));
        System.out.println("Sub Tag by type: " + Arrays.toString(Sub.class.getAnnotationsByType(Tag.class)));
        System.out.println("Sub Tag declared by type: " + Arrays.toString(Sub.class.getDeclaredAnnotationsByType(Tag.class)));
        System.out.println("Sub2 annotations: " + sorted(Sub2.class.getAnnotations()));
        System.out.println("Sub2 Tag by type: " + Arrays.toString(Sub2.class.getAnnotationsByType(Tag.class)));
        System.out.println("Sub2 Inh: " + Sub2.class.getAnnotation(Inh.class));
        System.out.println("ClassOnly visible: " + Base.class.isAnnotationPresent(ClassOnly.class));

        Many m = WithMany.class.getAnnotation(Many.class);
        System.out.println("Many default: " + m);
        Many mc = WithManyCustom.class.getAnnotation(Many.class);
        System.out.println("Many custom: " + mc);
        System.out.println("Many equals self-class: " + m.equals(WithMany.class.getAnnotation(Many.class))
                + " custom: " + m.equals(mc) + " hash stable: " + (m.hashCode() == WithMany.class.getAnnotation(Many.class).hashCode()));
        // (No hash VALUES: a Class or enum member hashes by identity.)
        System.out.println("Tag hash: " + Sub2.class.getAnnotationsByType(Tag.class)[0].hashCode()
                + " NotInh hash: " + Base.class.getAnnotation(NotInh.class).hashCode());
        System.out.println("annotationType: " + m.annotationType().getName() + " proxy: " + Proxy.isProxyClass(m.getClass()));
        System.out.println("Many.str default: " + Many.class.getDeclaredMethod("str").getDefaultValue());
        System.out.println("Many.tag default: " + Many.class.getDeclaredMethod("tag").getDefaultValue());
        System.out.println("Many.arr default: " + Arrays.toString((int[]) Many.class.getDeclaredMethod("arr").getDefaultValue()));
        System.out.println("Tag meta: " + sorted(Tag.class.getAnnotations()));
        System.out.println("TU meta: " + sorted(TU.class.getAnnotations()));

        Constructor<?> ic = Inner.class.getDeclaredConstructor(L3W98ReflectAnnotations.class, int.class, String.class);
        System.out.println("Inner ctor param annotations: " + params(ic.getParameterAnnotations()));
        for (Parameter p : ic.getParameters()) {
            System.out.println("  inner param " + p.getType().getSimpleName() + " " + sorted(p.getAnnotations()));
        }
        for (Constructor<?> k : En.class.getDeclaredConstructors()) {
            System.out.println("En ctor " + k.getParameterCount() + " annotations: " + params(k.getParameterAnnotations()));
        }

        System.out.println("Rec components:");
        for (RecordComponent rc : Rec.class.getRecordComponents()) {
            System.out.println("  " + rc + " generic=" + rc.getGenericType() + " sig=" + rc.getGenericSignature()
                    + " ann=" + sorted(rc.getAnnotations()) + " typeAnn=" + rc.getAnnotatedType()
                    + " accessor=" + rc.getAccessor());
            Field f = Rec.class.getDeclaredField(rc.getName());
            System.out.println("    field " + f.toGenericString() + " ann=" + sorted(f.getAnnotations()) + " typeAnn=" + f.getAnnotatedType());
            System.out.println("    accessor ann=" + sorted(rc.getAccessor().getAnnotations()));
        }
        Constructor<?> canon = Rec.class.getDeclaredConstructor(List.class, int.class);
        System.out.println("Rec canonical ctor params: " + params(canon.getParameterAnnotations())
                + " names=" + Arrays.toString(canon.getParameters()));

        Method tu = L3W98ReflectAnnotations.class.getDeclaredMethod("tu", Map.class);
        System.out.println("tu return: " + tu.getAnnotatedReturnType());
        System.out.println("tu param: " + Arrays.toString(tu.getAnnotatedParameterTypes()));
        System.out.println("tu exceptions: " + Arrays.toString(tu.getAnnotatedExceptionTypes()));
        TypeVariable<Method> t = tu.getTypeParameters()[0];
        System.out.println("tu T annotations: " + sorted(t.getAnnotations()) + " bounds: " + Arrays.toString(t.getAnnotatedBounds()));
        System.out.println("tu param ann: " + params(tu.getParameterAnnotations()));
        System.out.println("tu receiver: " + tu.getAnnotatedReceiverType());
        Method inst = Inner.class.getDeclaredMethods().length == 0 ? null : Inner.class.getDeclaredMethods()[0];
        System.out.println("Inner methods: " + (inst == null ? "none" : inst.getName()));
        System.out.println("Sub superclass annotated: " + Sub.class.getAnnotatedSuperclass());
    }
}
