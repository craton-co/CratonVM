// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-record-component-annotations-answer-empty-FIXED-20261022.md:
// a record component's declaration and type annotations, beyond the wave-57
// probe: member values of every kind, an annotation javac propagates to the
// component, its field and its accessor alike (the three must agree:
// `equals`, `hashCode`, `toString`), an array component's type annotation,
// and a component with none. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// declared: [@L3W58RecordComponentAnnotations.All(b=(byte)0x01, c='x', cls=java.lang.String.class, d=2.5, e=SECONDS, l=7L, nested=@L3W58RecordComponentAnnotations.Tag("n"), s="str", values={1, 2})]
// field-same: true true
// accessor-same: true true
// array-type: [@L3W58RecordComponentAnnotations.Use("arr")]
// array-elem: []
// none: [] [] null
// count: 2 2 1
// declared-again: [@L3W58RecordComponentAnnotations.All(b=(byte)0x01, c='x', cls=java.lang.String.class, d=2.5, e=SECONDS, l=7L, nested=@L3W58RecordComponentAnnotations.Tag("n"), s="str", values={1, 2})]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W58RecordComponentAnnotations`.
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.AnnotatedArrayType;
import java.lang.reflect.RecordComponent;
import java.util.Arrays;
import java.util.concurrent.TimeUnit;

public class L3W58RecordComponentAnnotations {
    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE_USE)
    @interface Use {
        String value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @interface Tag {
        String value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.RECORD_COMPONENT, ElementType.FIELD, ElementType.METHOD})
    @interface All {
        byte b();

        char c();

        Class<?> cls();

        double d();

        TimeUnit e();

        long l();

        Tag nested();

        String s();

        int[] values();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.RECORD_COMPONENT)
    @interface Mark {
    }

    public record Rec(
            @All(b = 1, c = 'x', cls = String.class, d = 2.5, e = TimeUnit.SECONDS, l = 7L,
                    nested = @Tag("n"), s = "str", values = {1, 2}) @Mark String name,
            int @Use("arr") [] arr,
            long none) {
    }

    public static void main(String[] args) throws Exception {
        RecordComponent[] rc = Rec.class.getRecordComponents();
        System.out.println("declared: " + Arrays.toString(rc[0].getAnnotationsByType(All.class)));
        All onComponent = rc[0].getAnnotation(All.class);
        All onField = Rec.class.getDeclaredField("name").getAnnotation(All.class);
        All onAccessor = rc[0].getAccessor().getAnnotation(All.class);
        System.out.println("field-same: " + onComponent.equals(onField) + " "
                + (onComponent.hashCode() == onField.hashCode()));
        System.out.println("accessor-same: " + onAccessor.equals(onComponent) + " "
                + onAccessor.toString().equals(onComponent.toString()));
        AnnotatedArrayType at = (AnnotatedArrayType) rc[1].getAnnotatedType();
        System.out.println("array-type: " + Arrays.toString(at.getAnnotations()));
        System.out.println("array-elem: " + Arrays.toString(at.getAnnotatedGenericComponentType().getAnnotations()));
        System.out.println("none: " + Arrays.toString(rc[2].getAnnotations()) + " "
                + Arrays.toString(rc[2].getAnnotatedType().getAnnotations()) + " "
                + rc[2].getAnnotation(All.class));
        System.out.println("count: " + rc[0].getAnnotations().length + " "
                + rc[0].getDeclaredAnnotations().length + " "
                + Rec.class.getDeclaredField("name").getAnnotations().length);
        System.out.println("declared-again: "
                + Arrays.toString(Rec.class.getRecordComponents()[0].getAnnotationsByType(All.class)));
    }
}
