import java.lang.annotation.*;
import java.lang.classfile.*;
import java.lang.classfile.attribute.*;
import java.lang.constant.*;
import java.lang.reflect.*;
import java.util.*;
import java.util.function.*;

/**
 * Interpreter round i5 wave 101, lane L3: a hunt sweep of generics and
 * annotations against HotSpot 25, both modes. The JDK's own parsers
 * (SignatureParser, AnnotationParser, TypeAnnotationParser) run in both
 * modes; what CratonVM supplies is the raw attribute bytes, the constant-pool
 * natives, getGenericSignature0, getParameters0, the record/sealed/nest
 * natives and the member mints. Odd class files come from java.lang.classfile.
 * Every line must match HotSpot; a differing row names its own suspect.
 */
public class L3W101GenericsAnnotationsSweep {
    // ---- generics -------------------------------------------------------
    interface Shape<T extends Number & Comparable<T>> { }
    static class Base<K, V extends List<? super K>> { }
    static class Gen<E extends Enum<E>> extends Base<String, ArrayList<? super String>>
            implements Shape<Integer>, Comparable<Gen<E>>, Supplier<Map<? extends E, ? super List<int[]>[]>> {
        public int compareTo(Gen<E> o) { return 0; }
        public Map<? extends E, ? super List<int[]>[]> get() { return null; }
        <X extends Throwable> void thrower(List<? extends X>[] xs, X... more) throws X { }
        class Inner<Q> { Map<E, Q> m; }
    }

    // ---- annotations ----------------------------------------------------
    public enum Mode { A, B }

    @Retention(RetentionPolicy.RUNTIME)
    public @interface Holder {
        Class<?> cls() default Object.class;
        Mode e() default Mode.A;
        int n() default 0;
        String[] tags() default {};
    }

    @Retention(RetentionPolicy.RUNTIME) @Repeatable(Tags.class) @Inherited
    public @interface Tag { String value(); }
    @Retention(RetentionPolicy.RUNTIME) @Inherited
    public @interface Tags { Tag[] value(); }

    @Retention(RetentionPolicy.RUNTIME) @Target({ElementType.PARAMETER, ElementType.TYPE_USE})
    public @interface P { int value() default 0; }

    @Tag("a") @Tag("b") static class Tagged { }
    @Tag("c") static class SubTagged extends Tagged { }
    static class SubInherit extends Tagged { }
    @Tags({@Tag("x")}) @Tag("y") static class Mixed { }

    class InnerCtor { InnerCtor(@P(1) int a, @P(2) String b) { } }
    enum EnumCtor {
        ONE(1);
        EnumCtor(@P(3) int v) { }
    }

    record Rec(@P(4) int a, List<@P(5) String> b) { }

    static @P(6) List<@P(7) String> annotatedReturn() { return null; }

    sealed interface Sealed permits SealedA, SealedB { }
    static final class SealedA implements Sealed { }
    static final class SealedB implements Sealed { }

    // ---- generated classes ---------------------------------------------
    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        Loader() { super(L3W101GenericsAnnotationsSweep.class.getClassLoader()); }
        @Override protected Class<?> findClass(String n) throws ClassNotFoundException {
            byte[] b = bytes.get(n);
            if (b == null) throw new ClassNotFoundException(n);
            return defineClass(n, b, 0, b.length);
        }
    }

    static final ClassDesc HOLDER = ClassDesc.of(L3W101GenericsAnnotationsSweep.class.getName() + "$Holder");
    static final ClassDesc MODE = ClassDesc.of(L3W101GenericsAnnotationsSweep.class.getName() + "$Mode");

    static byte[] annotated(String name, AnnotationElement... elements) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            cb.with(RuntimeVisibleAnnotationsAttribute.of(java.lang.classfile.Annotation.of(HOLDER, elements)));
        });
    }

    static byte[] badSignatures(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            cb.with(SignatureAttribute.of(cb.constantPool().utf8Entry("Ljava/lang/Object;Ljava/util/List<")));
            cb.withField("f", ConstantDescs.CD_Object, fb -> {
                fb.withFlags(ClassFile.ACC_PUBLIC);
                fb.with(SignatureAttribute.of(fb.constantPool().utf8Entry("TT")));
            });
            cb.withMethod("m", MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, mb -> {
                mb.with(SignatureAttribute.of(mb.constantPool().utf8Entry("(Ljava/util/List<*>V")));
                mb.withCode(c -> c.return_());
            });
            // A type variable no declaration binds.
            cb.withMethod("unbound", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, mb -> {
                mb.with(SignatureAttribute.of(mb.constantPool().utf8Entry("()TZ;")));
                mb.withCode(c -> c.aconst_null().areturn());
            });
            // A parameterized type whose class is missing.
            cb.withMethod("missing", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, mb -> {
                mb.with(SignatureAttribute.of(mb.constantPool().utf8Entry("()Lnope/Gone<Ljava/lang/String;>;")));
                mb.withCode(c -> c.aconst_null().areturn());
            });
        });
    }

    static byte[] sealedMissing(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_ABSTRACT | ClassFile.ACC_INTERFACE);
            cb.withSuperclass(ConstantDescs.CD_Object);
            cb.with(PermittedSubclassesAttribute.ofSymbols(ClassDesc.of(name + "Gone"), ClassDesc.of(name + "Here")));
        });
    }

    static byte[] sealedImpl(String name, String iface) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(ClassDesc.of(iface));
        });
    }

    // ---- driver ---------------------------------------------------------
    static void row(String label, Callable<Object> c) {
        String line;
        try {
            Object v = c.call();
            line = v instanceof Object[] a ? Arrays.deepToString(a) : String.valueOf(v);
        } catch (Throwable t) {
            line = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + " -> " + line);
    }

    interface Callable<T> { T call() throws Throwable; }

    static String bounds(TypeVariable<?>[] vs) {
        StringBuilder sb = new StringBuilder();
        for (TypeVariable<?> v : vs) sb.append(v.getName()).append(Arrays.toString(v.getBounds())).append(' ');
        return sb.toString().trim();
    }

    public static void main(String[] args) throws Exception {
        // generics
        row("gen.super", () -> Gen.class.getGenericSuperclass());
        row("gen.ifaces", () -> Gen.class.getGenericInterfaces());
        row("gen.tparams", () -> bounds(Gen.class.getTypeParameters()));
        row("shape.tparams", () -> bounds(Shape.class.getTypeParameters()));
        row("base.tparams", () -> bounds(Base.class.getTypeParameters()));
        Method thrower = Gen.class.getDeclaredMethod("thrower", List[].class, Throwable[].class);
        row("thrower.generic", () -> thrower.toGenericString());
        row("thrower.params", () -> thrower.getGenericParameterTypes());
        row("thrower.exc", () -> thrower.getGenericExceptionTypes());
        row("get.ret", () -> Gen.class.getMethod("get").getGenericReturnType());
        row("inner.field", () -> Gen.Inner.class.getDeclaredField("m").getGenericType());
        row("inner.owner", () -> ((ParameterizedType) Gen.Inner.class.getDeclaredField("m").getGenericType()).getOwnerType());
        row("wildcard", () -> {
            ParameterizedType p = (ParameterizedType) Gen.class.getMethod("get").getGenericReturnType();
            WildcardType w = (WildcardType) p.getActualTypeArguments()[1];
            return w + " lower=" + Arrays.toString(w.getLowerBounds()) + " upper=" + Arrays.toString(w.getUpperBounds());
        });

        Loader l = new Loader();
        l.bytes.put("GS0", badSignatures("GS0"));
        Class<?> gs = Class.forName("GS0", false, l);
        row("bad.class.super", () -> gs.getGenericSuperclass());
        row("bad.class.ifaces", () -> gs.getGenericInterfaces());
        row("bad.class.tparams", () -> gs.getTypeParameters());
        row("bad.field", () -> gs.getField("f").getGenericType());
        row("bad.field.toGeneric", () -> gs.getField("f").toGenericString());
        row("bad.method", () -> gs.getMethod("m", Object.class).getGenericParameterTypes());
        row("bad.method.toString", () -> gs.getMethod("m", Object.class).toString());
        row("unbound.tvar", () -> gs.getMethod("unbound").getGenericReturnType());
        row("missing.ptype", () -> gs.getMethod("missing").getGenericReturnType());

        // annotations
        l.bytes.put("AG0", annotated("AG0",
                AnnotationElement.of("cls", AnnotationValue.ofClass(ClassDesc.of("Missing0")))));
        l.bytes.put("AG1", annotated("AG1",
                AnnotationElement.of("e", AnnotationValue.ofEnum(MODE, "GONE"))));
        l.bytes.put("AG2", annotated("AG2",
                AnnotationElement.of("n", AnnotationValue.ofString("x"))));
        l.bytes.put("AG3", annotated("AG3",
                AnnotationElement.of("tags", AnnotationValue.ofInt(5))));
        l.bytes.put("AG4", annotated("AG4",
                AnnotationElement.of("nope", AnnotationValue.ofInt(5))));
        for (String n : new String[] {"AG0", "AG1", "AG2", "AG3", "AG4"}) {
            Class<?> c = Class.forName(n, false, l);
            row(n + ".toString", () -> c.getAnnotation(Holder.class));
            row(n + ".cls", () -> c.getAnnotation(Holder.class).cls());
            row(n + ".e", () -> c.getAnnotation(Holder.class).e());
            row(n + ".n", () -> c.getAnnotation(Holder.class).n());
            row(n + ".tags", () -> c.getAnnotation(Holder.class).tags());
        }

        row("byType.tagged", () -> Tagged.class.getAnnotationsByType(Tag.class));
        row("byType.sub", () -> SubTagged.class.getAnnotationsByType(Tag.class));
        row("byType.inherit", () -> SubInherit.class.getAnnotationsByType(Tag.class));
        row("byType.mixed", () -> Mixed.class.getAnnotationsByType(Tag.class));
        row("declared.mixed", () -> Mixed.class.getDeclaredAnnotations());
        row("sub.annotations", () -> SubTagged.class.getAnnotations());

        Constructor<?> ic = InnerCtor.class.getDeclaredConstructors()[0];
        row("inner.ctor.pann", () -> ic.getParameterAnnotations());
        row("inner.ctor.params", () -> {
            StringBuilder sb = new StringBuilder();
            for (Parameter p : ic.getParameters())
                sb.append(p.getName()).append(':').append(p.isImplicit()).append(':').append(p.isSynthetic())
                  .append(':').append(Arrays.toString(p.getAnnotations())).append(' ');
            return sb.toString().trim();
        });
        Constructor<?> ec = EnumCtor.class.getDeclaredConstructors()[0];
        row("enum.ctor", () -> ec.toGenericString());
        row("enum.ctor.pann", () -> ec.getParameterAnnotations());
        row("enum.ctor.params", () -> {
            StringBuilder sb = new StringBuilder();
            for (Parameter p : ec.getParameters())
                sb.append(p.getName()).append(':').append(p.isImplicit()).append(':').append(p.isSynthetic())
                  .append(':').append(Arrays.toString(p.getAnnotations())).append(' ');
            return sb.toString().trim();
        });
        row("enum.valueOf.params", () -> {
            StringBuilder sb = new StringBuilder();
            for (Parameter p : EnumCtor.class.getMethod("valueOf", String.class).getParameters())
                sb.append(p.getName()).append(':').append(p.isNamePresent()).append(':').append(p.getModifiers()).append(' ');
            return sb.toString().trim();
        });

        Method ar = L3W101GenericsAnnotationsSweep.class.getDeclaredMethod("annotatedReturn");
        row("annotated.return", () -> ar.getAnnotatedReturnType());
        row("annotated.return.args", () -> ((AnnotatedParameterizedType) ar.getAnnotatedReturnType()).getAnnotatedActualTypeArguments());
        row("annotated.return.type", () -> ar.getAnnotatedReturnType().getType());

        row("rec.components", () -> {
            StringBuilder sb = new StringBuilder();
            for (RecordComponent rc : Rec.class.getRecordComponents())
                sb.append(rc).append('|').append(rc.getGenericSignature()).append('|').append(rc.getAccessor())
                  .append('|').append(Arrays.toString(rc.getAnnotations())).append('|')
                  .append(rc.getAnnotatedType()).append(' ');
            return sb.toString().trim();
        });
        row("rec.ctor.pann", () -> Rec.class.getDeclaredConstructors()[0].getParameterAnnotations());

        row("sealed.permits", () -> Sealed.class.getPermittedSubclasses());
        l.bytes.put("SM0", sealedMissing("SM0"));
        l.bytes.put("SM0Here", sealedImpl("SM0Here", "SM0"));
        row("sealed.missing", () -> Class.forName("SM0", false, l).getPermittedSubclasses());
        row("sealed.missing.isSealed", () -> Class.forName("SM0", false, l).isSealed());

        // enum valueOf texts
        enum Local { L1 }
        row("valueOf.nested", () -> Mode.valueOf("Z"));
        row("valueOf.local", () -> Local.valueOf("Z"));
        row("valueOf.null", () -> Mode.valueOf(null));
        row("valueOf.enum", () -> Enum.valueOf(Mode.class, "Z"));
        row("valueOf.raw", () -> {
            @SuppressWarnings({"rawtypes", "unchecked"})
            Object o = Enum.valueOf((Class) String.class, "Z");
            return o;
        });
    }
}
