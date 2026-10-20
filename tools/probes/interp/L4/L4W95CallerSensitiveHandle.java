// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 95, lane L4: a caller-sensitive JDK method reached
// through a `Lookup` finder (`CRATONVM_MHN_CALLER_SENSITIVE_FLAG`, default on,
// `=0` off; L3's `L3W94CallerSensitiveMatrix` row `get-other-handle`).
//
// HotSpot's `MethodHandleNatives.resolve` / `init` set `MN_CALLER_SENSITIVE` on
// a `@CallerSensitive` method of a boot / platform class
// (`init_method_MemberName`). `Lookup.findVirtual` / `findStatic` /
// `unreflect` then:
//   * refuse it to a lookup without ORIGINAL access (`findBoundCallerLookup`:
//     "Attempt to lookup caller-sensitive method using restricted lookup object");
//   * bind it to the lookup class (`maybeBindCaller` -> `MethodHandleImpl.BindCaller`):
//     its caller-sensitive adapter with the lookup class inserted when the class
//     declares one (`Class.forName(String)` -> `forName(String, Class)`), else an
//     injected invoker `<lookup class>$$InjectedInvoker/0x..`, a hidden NESTMATE
//     of the lookup class, which is then the method's caller.
// This VM never set the bit: the plain handle called `Field.get` with the
// lookup class as the caller, and a restricted lookup got the handle.
//
// Output: one line per row; a throwable prints its class and message, with
// hidden-class suffixes (`/0x1a2b`) erased to `/0x`.
//
// Run: javac -d . L4W95CallerSensitiveHandle.java
//      java -cp . L4W95CallerSensitiveHandle
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W95CallerSensitiveHandle
//      CRATONVM_MHN_CALLER_SENSITIVE_FLAG=0 cratonvm ...    (the old answers)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from the JDK 25 sources: MethodHandles.Lookup.findBoundCallerLookup / maybeBindCaller, MethodHandleImpl.BindCaller, Reflection.verifyMemberAccess)
// get-other-handle: java.lang.IllegalAccessException: class L4W95CallerSensitiveHandle$$InjectedInvoker/0x cannot access a member of class L4W95CSOther with modifiers "private"
// get-own-handle: 5
// get-public-handle: 9
// unreflect-other: java.lang.IllegalAccessException: class L4W95CallerSensitiveHandle$$InjectedInvoker/0x cannot access a member of class L4W95CSOther with modifiers "private"
// public-lookup-find: java.lang.IllegalAccessException: Attempt to lookup caller-sensitive method using restricted lookup object
// in-lookup-find: java.lang.IllegalAccessException: Attempt to lookup caller-sensitive method using restricted lookup object
// forname-handle: class java.lang.String
// reveal: invokeVirtual java.lang.reflect.Field.get:(Object)Object
// plain-public-lookup: 3
// HOTSPOT_EXPECTED_END
//
// Least certain: `get-own-handle` (the injected invoker is defined with
// `NESTMATE_CLASS`, so it may read this class's private field: JDK 25
// `BindCaller.makeInjectedInvoker`); `in-lookup-find` (`Lookup.in` drops
// ORIGINAL: JDK 25 `Lookup.in` -> `newModes &= ~ORIGINAL`).
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Field;

class L4W95CSOther {
    private int secret = 4;
    public int open = 9;
}

public class L4W95CallerSensitiveHandle {
    private int mine = 5;

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            Throwable c = t;
            if (c instanceof java.lang.reflect.InvocationTargetException && c.getCause() != null) {
                c = c.getCause();
            }
            out = c.getClass().getName() + ": " + c.getMessage();
        }
        System.out.println(name + ": " + out.replaceAll("/0x[0-9a-fA-F]+", "/0x"));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType getType = MethodType.methodType(Object.class, Object.class);
        MethodHandle getH = lookup.findVirtual(Field.class, "get", getType);
        Field other = L4W95CSOther.class.getDeclaredField("secret");
        Field own = L4W95CallerSensitiveHandle.class.getDeclaredField("mine");
        Field open = L4W95CSOther.class.getField("open");
        L4W95CSOther o = new L4W95CSOther();
        L4W95CallerSensitiveHandle self = new L4W95CallerSensitiveHandle();

        row("get-other-handle", () -> getH.invoke(other, o));
        row("get-own-handle", () -> getH.invoke(own, self));
        row("get-public-handle", () -> getH.invoke(open, o));
        row("unreflect-other", () -> {
            MethodHandle h = lookup.unreflect(Field.class.getMethod("get", Object.class));
            return h.invoke(other, o);
        });
        row("public-lookup-find", () -> MethodHandles.publicLookup().findVirtual(Field.class, "get", getType));
        row("in-lookup-find", () -> lookup.in(Object.class).findVirtual(Field.class, "get", getType));
        row("forname-handle", () -> {
            MethodHandle h = lookup.findStatic(Class.class, "forName",
                    MethodType.methodType(Class.class, String.class));
            return h.invoke("java.lang.String");
        });
        row("reveal", () -> lookup.revealDirect(getH));
        row("plain-public-lookup", () -> {
            MethodHandle h = MethodHandles.publicLookup().findVirtual(String.class, "length",
                    MethodType.methodType(int.class));
            return h.invoke("abc");
        });
    }
}
