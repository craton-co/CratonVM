// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L3 --
// docs/known-issues/interpreter/i53-L3-proposal-the-served-raise-mark-keeps-the-receivers-class-20261017.md:
// a native serving an INTERFACE call raises, and HotSpot lists the method
// the receiver's class selected (`TypeVariableImpl.getAnnotation`, not the
// interface's abstract `TypeVariable.getAnnotation`). The funnel's mark keeps
// the receiver's class (`ServedNativeRaise::receiver_class`) and the capture
// selects from it (`stackwalker::receiver_selected_method`).
//
// Each row prints the throwable's class and message, then the frames above
// the probe's first frame as `class.method`, then the first probe frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   tv-*            -- `TypeVariable.getAnnotation` / `getDeclaredAnnotation`
//                      / `getAnnotationsByType` / `getDeclaredAnnotationsByType`
//                      of `null`, through the interface;
//   element-field   -- `AnnotatedElement.getAnnotation(null)` on a `Field`;
//   element-class   -- `AnnotatedElement.getAnnotation(null)` on a `Class`;
//   member-field    -- `Field.getAnnotation(null)`, a direct call (control:
//                      the static selection already answers it).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// tv-getAnnotation: java.lang.NullPointerException null [java.util.Objects.requireNonNull sun.reflect.generics.reflectiveObjects.TypeVariableImpl.getAnnotation] L3W54ServedRaiseReceiverClass.lambda$main$0
// tv-getDeclaredAnnotation: java.lang.NullPointerException null [java.util.Objects.requireNonNull sun.reflect.generics.reflectiveObjects.TypeVariableImpl.getDeclaredAnnotation] L3W54ServedRaiseReceiverClass.lambda$main$1
// tv-getAnnotationsByType: java.lang.NullPointerException null [java.util.Objects.requireNonNull sun.reflect.generics.reflectiveObjects.TypeVariableImpl.getAnnotationsByType] L3W54ServedRaiseReceiverClass.lambda$main$2
// tv-getDeclaredAnnotationsByType: java.lang.NullPointerException null [java.util.Objects.requireNonNull sun.reflect.generics.reflectiveObjects.TypeVariableImpl.getDeclaredAnnotationsByType] L3W54ServedRaiseReceiverClass.lambda$main$3
// element-field: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Field.getAnnotation] L3W54ServedRaiseReceiverClass.lambda$main$4
// element-class: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getAnnotation] L3W54ServedRaiseReceiverClass.lambda$main$5
// member-field: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Field.getAnnotation] L3W54ServedRaiseReceiverClass.lambda$main$6
// tv-getAnnotation-again: java.lang.NullPointerException null [java.util.Objects.requireNonNull sun.reflect.generics.reflectiveObjects.TypeVariableImpl.getAnnotation] L3W54ServedRaiseReceiverClass.lambda$main$7
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 54 (`--compatible`, where natives serve these
// methods): the `tv-*` and `element-*` rows printed `[]` -- the capture
// resolved the interface's abstract method and listed nothing. `--jdk-only`
// runs the JDK's bytecode for most of them and lists the frames itself.
// Positive control: `CRATONVM_FRAME_TRACE=1` prints
//     [SERVED_RAISE_RECEIVER] java/lang/reflect/TypeVariable.getAnnotation... from receiver: sun/reflect/generics/reflectiveObjects/TypeVariableImpl.getAnnotation
// for the `tv-getAnnotation` row (under CRATONVM_THROWABLE_SERVED_RAISE_RECEIVER,
// on from lane L3's last wave-54 commit).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W54ServedRaiseReceiverClass`.
import java.lang.reflect.AnnotatedElement;
import java.lang.reflect.Field;
import java.lang.reflect.TypeVariable;

public class L3W54ServedRaiseReceiverClass<T> {
    public int f;

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            r.run();
            System.out.println(name + ": no exception");
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int k = 0;
            while (k < st.length && !st[k].getClassName().startsWith("L3W54")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            String first = k < st.length ? st[k].getClassName() + "." + st[k].getMethodName() : "-";
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
        }
    }

    public static void main(String[] args) throws Exception {
        TypeVariable<?> tv = L3W54ServedRaiseReceiverClass.class.getTypeParameters()[0];
        Field field = L3W54ServedRaiseReceiverClass.class.getField("f");
        AnnotatedElement elementField = field;
        AnnotatedElement elementClass = L3W54ServedRaiseReceiverClass.class;
        row("tv-getAnnotation", () -> tv.getAnnotation(null));
        row("tv-getDeclaredAnnotation", () -> tv.getDeclaredAnnotation(null));
        row("tv-getAnnotationsByType", () -> tv.getAnnotationsByType(null));
        row("tv-getDeclaredAnnotationsByType", () -> tv.getDeclaredAnnotationsByType(null));
        row("element-field", () -> elementField.getAnnotation(null));
        row("element-class", () -> elementClass.getAnnotation(null));
        row("member-field", () -> field.getAnnotation(null));
        row("tv-getAnnotation-again", () -> tv.getAnnotation(null));
    }
}
