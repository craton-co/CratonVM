// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L3 (review) --
// docs/internal/fixed-bugs/interpreter-L3-record-component-annotations-answer-empty-FIXED-20261022.md:
// a record component's declaration and type annotations. HotSpot hands the
// `RecordComponent` the `Record` attribute's per-component
// `RuntimeVisibleAnnotations` / `RuntimeVisibleTypeAnnotations` bytes; the
// JDK's bodies parse them. The first row is repeated last (cold-VM gap).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// type: [@L3W57RecordComponentAnnotations.Use("rc")]
// type-arg: [@L3W57RecordComponentAnnotations.Use("rc-arg")]
// declared: [@L3W57RecordComponentAnnotations.Comp("c")]
// get: @L3W57RecordComponentAnnotations.Comp("c")
// plain: [] 0
// type-again: [@L3W57RecordComponentAnnotations.Use("rc")]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W57RecordComponentAnnotations`.
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.AnnotatedParameterizedType;
import java.lang.reflect.AnnotatedType;
import java.lang.reflect.RecordComponent;
import java.util.Arrays;
import java.util.List;

public class L3W57RecordComponentAnnotations {
    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE_USE)
    @interface Use {
        String value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.RECORD_COMPONENT)
    @interface Comp {
        String value();
    }

    public record Rec(@Comp("c") @Use("rc") String name, List<@Use("rc-arg") Integer> ids, int plain) {
    }

    static String ann(AnnotatedType t) {
        return Arrays.toString(t.getAnnotations());
    }

    public static void main(String[] args) {
        RecordComponent[] rc = Rec.class.getRecordComponents();
        System.out.println("type: " + ann(rc[0].getAnnotatedType()));
        System.out.println("type-arg: " + ann(((AnnotatedParameterizedType) rc[1].getAnnotatedType())
                .getAnnotatedActualTypeArguments()[0]));
        System.out.println("declared: " + Arrays.toString(rc[0].getDeclaredAnnotations()));
        System.out.println("get: " + rc[0].getAnnotation(Comp.class));
        System.out.println("plain: " + ann(rc[2].getAnnotatedType()) + " " + rc[2].getAnnotations().length);
        System.out.println("type-again: " + ann(Rec.class.getRecordComponents()[0].getAnnotatedType()));
    }
}
