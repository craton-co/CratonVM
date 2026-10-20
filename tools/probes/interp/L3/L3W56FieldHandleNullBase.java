// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-a-field-accessors-method-handle-raise-lists-no-checkbase-frames-FIXED-20261020.md:
// a field getter's or setter's method handle given a null base raises in
// `DirectMethodHandle.checkBase`'s `Objects.requireNonNull`, below the
// handle's hidden `LambdaForm` frames. The VM's method-handle door serves the
// caller's `invokeExact` / `invoke` and raised with no JDK frame; it now notes
// a chain whose first hop is the unlisted invoker
// (`ServedRaiseHopSite::Hidden`, `CRATONVM_THROWABLE_SERVED_RAISE_HIDDEN_INVOKER`).
//
// Each row prints the throwable's class and message, the frames above the
// probe's first frame as `class.method`, then the first probe frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   getter-exact / getter-invoke   -- `findGetter(int)`'s `invokeExact((Box) null)`
//                                     and `invoke((Object) null)`;
//   getter-object                  -- a reference field's getter;
//   setter-exact                   -- `findSetter(int)`'s `invokeExact((Box) null, 5)`;
//   field-get                      -- `Field.get(null)` with the override (the JDK
//                                     accessor's own `invokeExact` under `--jdk-only`);
//   getter-with-arguments          -- `invokeWithArguments((Object) null)`, Java HotSpot
//                                     lists above the hidden frames;
//   bound-control                  -- a getter called with a live base (no raise).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// getter-exact: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase] L3W56FieldHandleNullBase.lambda$main$0
// getter-invoke: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase] L3W56FieldHandleNullBase.lambda$main$1
// getter-object: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase] L3W56FieldHandleNullBase.lambda$main$2
// setter-exact: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase] L3W56FieldHandleNullBase.lambda$main$3
// field-get: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.get java.lang.reflect.Field.get] L3W56FieldHandleNullBase.lambda$main$4
// getter-with-arguments: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase java.lang.invoke.MethodHandle.invokeWithArguments] L3W56FieldHandleNullBase.lambda$main$5
// bound-control: 7
// getter-exact-again: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase] L3W56FieldHandleNullBase.lambda$main$6
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 56: the handle rows printed `[]`, both modes; the
// `field-get` row printed `[...getInt ...get Field.get]` under `--jdk-only`.
// Positive control: `CRATONVM_FRAME_TRACE=1` prints
//     [SERVED_RAISE_FRAMES] java/lang/invoke/MethodHandle.invokeExact(LL3W56FieldHandleNullBase$Box;)I (hidden invoker) raised java/lang/NullPointerException (chain of 3): 2 frame(s)
// for the first row.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W56FieldHandleNullBase`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Field;

public class L3W56FieldHandleNullBase {
    static class Box {
        public int x = 7;
        public String s = "s";
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            Object v = r.run();
            System.out.println(name + ": " + v);
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int k = 0;
            while (k < st.length && !st[k].getClassName().startsWith("L3W56")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            String first = k < st.length ? st[k].getClassName() + "." + st[k].getMethodName() : "-";
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle getter = lookup.findGetter(Box.class, "x", int.class);
        MethodHandle getterObject = lookup.findGetter(Box.class, "s", String.class);
        MethodHandle setter = lookup.findSetter(Box.class, "x", int.class);
        Field field = Box.class.getField("x");
        field.setAccessible(true);
        row("getter-exact", () -> (int) getter.invokeExact((Box) null));
        row("getter-invoke", () -> (int) getter.invoke((Object) null));
        row("getter-object", () -> (String) getterObject.invokeExact((Box) null));
        row("setter-exact", () -> {
            setter.invokeExact((Box) null, 5);
            return "set";
        });
        row("field-get", () -> field.get(null));
        row("getter-with-arguments", () -> getter.invokeWithArguments((Object) null));
        System.out.println("bound-control: " + (int) getter.invokeExact(new Box()));
        row("getter-exact-again", () -> (int) getter.invokeExact((Box) null));
    }
}
