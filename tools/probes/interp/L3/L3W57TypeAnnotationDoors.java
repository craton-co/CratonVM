// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-type-annotations-answer-empty-on-several-doors-FIXED-20261021.md:
// `TYPE_USE` annotations through the doors the wave-56 probes do not reach,
// the guard for `CRATONVM_JDK_TYPE_ANNOTATIONS` (the JDK's
// `TypeAnnotationParser` builds every `AnnotatedType`, in both modes): a
// JSpecify-style `@Nullable` on a return type (what the retired
// `getAnnotatedReturnType` intrinsic was for), nested and wildcard type
// arguments, an owner type, a `Parameter`, repeatable and CLASS-retained
// annotations, `AnnotatedType` equality and rendering. (A record
// component's are `L3W57RecordComponentAnnotations`'.) The first row is
// repeated last (cold-VM gap).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// nullable-return: true @L3W57TypeAnnotationDoors.Nullable()
// plain-return: false []
// nested: key=[@L3W57TypeAnnotationDoors.Tag("k")] list=[] wild=[@L3W57TypeAnnotationDoors.Tag("v")] bound=[@L3W57TypeAnnotationDoors.Tag("bound")]
// owned: [@L3W57TypeAnnotationDoors.Tag("inner")] owner=L3W57TypeAnnotationDoors
// repeated: [@L3W57TypeAnnotationDoors.Tags({@L3W57TypeAnnotationDoors.Tag("a"), @L3W57TypeAnnotationDoors.Tag("b")})] byType=2 declared=1
// mixed: [@L3W57TypeAnnotationDoors.Tag("seen")]
// param: [@L3W57TypeAnnotationDoors.Nullable(), @L3W57TypeAnnotationDoors.Tag("p")]
// equality: true true same=false
// render: @L3W57TypeAnnotationDoors.Nullable() java.lang.String | java.util.Map<@L3W57TypeAnnotationDoors.Tag("k") java.lang.String, java.util.List<@L3W57TypeAnnotationDoors.Tag("v") ? extends @L3W57TypeAnnotationDoors.Tag("bound") java.lang.Number>>
// classes: sun.reflect.annotation.AnnotatedTypeFactory$AnnotatedTypeBaseImpl sun.reflect.annotation.AnnotatedTypeFactory$AnnotatedParameterizedTypeImpl sun.reflect.annotation.AnnotatedTypeFactory$AnnotatedWildcardTypeImpl
// nullable-return-again: true @L3W57TypeAnnotationDoors.Nullable()
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W57TypeAnnotationDoors`.
import java.lang.annotation.ElementType;
import java.lang.annotation.Repeatable;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.AnnotatedParameterizedType;
import java.lang.reflect.AnnotatedType;
import java.lang.reflect.AnnotatedWildcardType;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.List;
import java.util.Map;

public class L3W57TypeAnnotationDoors {
    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE_USE)
    @interface Nullable {
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE_USE)
    @Repeatable(Tags.class)
    @interface Tag {
        String value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE_USE)
    @interface Tags {
        Tag[] value();
    }

    @Retention(RetentionPolicy.CLASS)
    @Target(ElementType.TYPE_USE)
    @interface Hidden {
    }

    public class Inner {
    }

    public @Nullable String find() {
        return null;
    }

    public String plain() {
        return null;
    }

    public Map<@Tag("k") String, List<@Tag("v") ? extends @Tag("bound") Number>> nested;
    public L3W57TypeAnnotationDoors.@Tag("inner") Inner owned;
    public @Tag("a") @Tag("b") String repeated;
    public @Hidden @Tag("seen") String mixed;

    public void take(@Nullable @Tag("p") String s) {
    }

    static String ann(AnnotatedType t) {
        return Arrays.toString(t.getAnnotations());
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = L3W57TypeAnnotationDoors.class;
        Method find = c.getMethod("find");
        System.out.println("nullable-return: " + find.getAnnotatedReturnType().isAnnotationPresent(Nullable.class)
                + " " + find.getAnnotatedReturnType().getAnnotation(Nullable.class));
        System.out.println("plain-return: " + c.getMethod("plain").getAnnotatedReturnType().isAnnotationPresent(Nullable.class)
                + " " + ann(c.getMethod("plain").getAnnotatedReturnType()));
        AnnotatedParameterizedType nested = (AnnotatedParameterizedType) c.getField("nested").getAnnotatedType();
        AnnotatedType[] nargs = nested.getAnnotatedActualTypeArguments();
        AnnotatedParameterizedType list = (AnnotatedParameterizedType) nargs[1];
        AnnotatedWildcardType wild = (AnnotatedWildcardType) list.getAnnotatedActualTypeArguments()[0];
        System.out.println("nested: key=" + ann(nargs[0]) + " list=" + ann(list) + " wild=" + ann(wild)
                + " bound=" + ann(wild.getAnnotatedUpperBounds()[0]));
        AnnotatedType owned = c.getField("owned").getAnnotatedType();
        System.out.println("owned: " + ann(owned) + " owner=" + (owned.getAnnotatedOwnerType() == null ? "null"
                : owned.getAnnotatedOwnerType().getType().getTypeName()));
        AnnotatedType rep = c.getField("repeated").getAnnotatedType();
        System.out.println("repeated: " + ann(rep) + " byType=" + rep.getAnnotationsByType(Tag.class).length
                + " declared=" + rep.getDeclaredAnnotations().length);
        System.out.println("mixed: " + ann(c.getField("mixed").getAnnotatedType()));
        System.out.println("param: " + ann(c.getMethod("take", String.class).getParameters()[0].getAnnotatedType()));
        AnnotatedType r1 = find.getAnnotatedReturnType();
        AnnotatedType r2 = find.getAnnotatedReturnType();
        System.out.println("equality: " + r1.equals(r2) + " " + (r1.hashCode() == r2.hashCode()) + " same=" + (r1 == r2));
        System.out.println("render: " + r1 + " | " + nested);
        System.out.println("classes: " + r1.getClass().getName() + " " + nested.getClass().getName()
                + " " + wild.getClass().getName());
        System.out.println("nullable-return-again: " + find.getAnnotatedReturnType().isAnnotationPresent(Nullable.class)
                + " " + find.getAnnotatedReturnType().getAnnotation(Nullable.class));
    }
}
