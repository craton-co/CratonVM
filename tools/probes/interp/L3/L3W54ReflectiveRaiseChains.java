// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L3 -- reflective raises HotSpot lists
// two or more JDK frames below the method a native serves, which the
// served-raise mark's one-hop rule cannot reach; the natives note the chain
// (`NativeExceptionAccess::note_served_raise_chain`):
//
//   enum-ctor   -- `Constructor.newInstance` of an enum constructor:
//                  `Constructor.acquireConstructorAccessor`'s throw, under
//                  `newInstanceWithCaller` (`lang_class::native_constructor_new_instance_body`);
//   array-*     -- `Array.newInstance(Class, int)` / `(Class, int...)`
//                  refused: the JDK `native` `Array.newArray` /
//                  `multiNewArray` frame under `newInstance` (the natives
//                  `--compatible` serves `newInstance` with).
//
// Each row prints the throwable's class and message, then the frames above
// the probe's first frame as `class.method`, then the first probe frame.
// The first row is repeated as the last, for a cold-VM gap.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// enum-ctor: java.lang.IllegalArgumentException Cannot reflectively create enum objects [java.lang.reflect.Constructor.acquireConstructorAccessor java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.Constructor.newInstance] L3W54ReflectiveRaiseChains.lambda$main$0
// array-negative: java.lang.NegativeArraySizeException -1 [java.lang.reflect.Array.newArray java.lang.reflect.Array.newInstance] L3W54ReflectiveRaiseChains.lambda$main$1
// array-void: java.lang.IllegalArgumentException null [java.lang.reflect.Array.newArray java.lang.reflect.Array.newInstance] L3W54ReflectiveRaiseChains.lambda$main$2
// array-null: java.lang.NullPointerException null [java.lang.reflect.Array.newArray java.lang.reflect.Array.newInstance] L3W54ReflectiveRaiseChains.lambda$main$3
// array-multi-negative: java.lang.NegativeArraySizeException -1 [java.lang.reflect.Array.multiNewArray java.lang.reflect.Array.newInstance] L3W54ReflectiveRaiseChains.lambda$main$4
// array-multi-empty: java.lang.IllegalArgumentException null [java.lang.reflect.Array.multiNewArray java.lang.reflect.Array.newInstance] L3W54ReflectiveRaiseChains.lambda$main$5
// enum-ctor-again: java.lang.IllegalArgumentException Cannot reflectively create enum objects [java.lang.reflect.Constructor.acquireConstructorAccessor java.lang.reflect.Constructor.newInstanceWithCaller java.lang.reflect.Constructor.newInstance] L3W54ReflectiveRaiseChains.lambda$main$6
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 54 (`--compatible`, read from the code): the caller on
// top of every row (`[]`). `--jdk-only` runs the JDK bodies of
// `Constructor.newInstance` and `Array.newInstance`.
// Positive control: `CRATONVM_FRAME_TRACE=1` prints
// `[SERVED_RAISE_FRAMES] ... (chain of <n>): <n> frame(s)` per row.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W54ReflectiveRaiseChains`.
import java.lang.reflect.Array;
import java.lang.reflect.Constructor;

public class L3W54ReflectiveRaiseChains {
    public enum E {
        A
    }

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
        Constructor<?> enumCtor = E.class.getDeclaredConstructors()[0];
        enumCtor.setAccessible(true);
        row("enum-ctor", () -> enumCtor.newInstance("B", 1));
        row("array-negative", () -> Array.newInstance(int.class, -1));
        row("array-void", () -> Array.newInstance(void.class, 1));
        row("array-null", () -> Array.newInstance(null, 1));
        row("array-multi-negative", () -> Array.newInstance(int.class, 2, -1));
        row("array-multi-empty", () -> Array.newInstance(int.class, new int[0]));
        row("enum-ctor-again", () -> enumCtor.newInstance("B", 1));
    }
}
