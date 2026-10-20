// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L3 -- the JEP 403 half of the
// caller-sensitive matrix (`L3W94CallerSensitiveMatrix`; stage 3 of
// docs/internal/fixed-bugs/interpreter-i49-L3-proposal-one-caller-class-rule-over-the-walkers-capture-DONE-20261128.md)
// under command-line grants:
//
//   --add-opens java.base/java.util=ALL-UNNAMED
//   --add-exports java.base/sun.security.provider=ALL-UNNAMED
//
// A caller the door natives misplace onto a JDK frame passes every row here
// as it should; a caller misplaced onto ANOTHER unnamed-module class passes
// too. The rows that can fail are the refusals a grant does not cover
// (java.lang is not opened; a private member of an exported package needs
// setAccessible) and the grants themselves reaching each shape.
//
// Rows (`<door>-<shape>`, shapes as in `L3W94CallerSensitiveMatrix`):
//   open-util-*         setAccessible(true) on java.util.ArrayList.size: ok;
//   try-util-direct     trySetAccessible on it: true;
//   get-util-opened     its value read after setAccessible, a 3-element list: 3;
//   open-lang-*         setAccessible(true) on java.lang.String.value (java.lang
//                       not opened): refused;
//   newinst-sha-*       Class.newInstance of sun.security.provider.SHA (now
//                       exported): the class name;
//   get-sha-private-*   Field.get of SHA's private `state` without
//                       setAccessible: IllegalAccessException (exported, so the
//                       "a member of" form);
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 sources; to be measured)
// open-util-direct: ok
// open-util-reflect: ok
// open-util-lambda: ok
// open-util-handle: ok
// try-util-direct: true
// get-util-opened: 3
// open-lang-direct: java.lang.reflect.InaccessibleObjectException: Unable to make field private final byte[] java.lang.String.value accessible: module java.base does not "opens java.lang" to unnamed module
// open-lang-reflect: java.lang.reflect.InaccessibleObjectException: Unable to make field private final byte[] java.lang.String.value accessible: module java.base does not "opens java.lang" to unnamed module
// open-lang-lambda: java.lang.reflect.InaccessibleObjectException: Unable to make field private final byte[] java.lang.String.value accessible: module java.base does not "opens java.lang" to unnamed module
// newinst-sha-direct: sun.security.provider.SHA
// newinst-sha-reflect: sun.security.provider.SHA
// newinst-sha-lambda: sun.security.provider.SHA
// get-sha-private-direct: java.lang.IllegalAccessException: class L3W94CallerSensitiveOpens cannot access a member of class sun.security.provider.SHA (in module java.base) with modifiers "private"
// get-sha-private-reflect: java.lang.IllegalAccessException: class L3W94CallerSensitiveOpens$$InjectedInvoker/0x cannot access a member of class sun.security.provider.SHA (in module java.base) with modifiers "private"
// HOTSPOT_EXPECTED_END
//
// Least certain: the `state` field's modifiers (`private int[] state` in JDK
// 25's SHA.java, read from the sources).
//
// SETUP: `java|cratonvm [--compatible] [--nojit] --add-opens java.base/java.util=ALL-UNNAMED
//   --add-exports java.base/sun.security.provider=ALL-UNNAMED -cp . L3W94CallerSensitiveOpens`,
// and each cratonvm run again with `CRATONVM_REFLECTION_CS_CALLER_RULE=1`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;

public class L3W94CallerSensitiveOpens {
    interface Call { Object run() throws Throwable; }
    interface FieldOpen { void open(Field f, boolean flag); }
    interface NewInst { Object make(Class<?> c) throws Exception; }

    static String norm(String s) {
        if (s == null) {
            return "null";
        }
        return s.replaceAll(" @[0-9a-f]+", "").replaceAll("/0x[0-9a-f]+", "/0x");
    }

    static void row(String label, Call call) {
        String out;
        try {
            out = norm(String.valueOf(call.run()));
        } catch (Throwable t) {
            Throwable c = t;
            while (c instanceof InvocationTargetException && c.getCause() != null) {
                c = c.getCause();
            }
            out = c.getClass().getName() + ": " + norm(c.getMessage());
        }
        System.out.println(label + ": " + out);
    }

    static Field size() throws NoSuchFieldException {
        return ArrayList.class.getDeclaredField("size");
    }

    static Field value() throws NoSuchFieldException {
        return String.class.getDeclaredField("value");
    }

    public static void main(String[] args) throws Throwable {
        Method fieldOpen = Field.class.getMethod("setAccessible", boolean.class);
        Method fieldGet = Field.class.getMethod("get", Object.class);
        Method classNew = Class.class.getMethod("newInstance");
        MethodHandle openH = MethodHandles.lookup().findVirtual(Field.class, "setAccessible",
                MethodType.methodType(void.class, boolean.class));
        FieldOpen openL = Field::setAccessible;
        @SuppressWarnings("deprecation")
        NewInst newL = Class::newInstance;
        Class<?> sha = Class.forName("sun.security.provider.SHA", false, null);

        row("open-util-direct", () -> { size().setAccessible(true); return "ok"; });
        row("open-util-reflect", () -> { fieldOpen.invoke(size(), true); return "ok"; });
        row("open-util-lambda", () -> { openL.open(size(), true); return "ok"; });
        row("open-util-handle", () -> { openH.invoke(size(), true); return "ok"; });
        row("try-util-direct", () -> size().trySetAccessible());
        row("get-util-opened", () -> {
            Field f = size();
            f.setAccessible(true);
            return f.get(new ArrayList<>(List.of(1, 2, 3)));
        });
        row("open-lang-direct", () -> { value().setAccessible(true); return "ok"; });
        row("open-lang-reflect", () -> { fieldOpen.invoke(value(), true); return "ok"; });
        row("open-lang-lambda", () -> { openL.open(value(), true); return "ok"; });
        row("newinst-sha-direct", () -> { @SuppressWarnings("deprecation") Object x = sha.newInstance(); return x.getClass().getName(); });
        row("newinst-sha-reflect", () -> classNew.invoke(sha).getClass().getName());
        row("newinst-sha-lambda", () -> newL.make(sha).getClass().getName());
        // Wave 96 (lane L3): without the SETUP grants this throws; print it as a
        // row (the message names `unnamed module @<identity hash>`, which differs
        // per VM run) instead of letting it escape `main` unnormalised.
        Object digest;
        try {
            digest = sha.getConstructor().newInstance();
        } catch (Throwable t) {
            System.out.println("make-sha: " + t.getClass().getName() + ": " + norm(t.getMessage()));
            return;
        }
        row("get-sha-private-direct", () -> sha.getDeclaredField("state").get(digest));
        row("get-sha-private-reflect", () -> fieldGet.invoke(sha.getDeclaredField("state"), digest));
    }
}
