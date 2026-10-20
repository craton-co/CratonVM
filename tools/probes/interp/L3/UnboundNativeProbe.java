// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 6, lane L3: a `native` method nothing implements
// raises `UnsatisfiedLinkError` naming the method the way HotSpot spells it,
// not `AbstractMethodError ... has no Code attribute`, and a STATIC unbound
// native no longer has its first argument treated as a receiver by
// `execute`'s no-`Code` rescues (`execute_entry.rs::execute_no_code_rescue`).
//
// Reached directly and through reflection (reflection enters the method
// through `interpreter::execute`, the door the fix is in; the direct calls
// show whether the invoke paths agree with it).
//
// Expected (HotSpot 25), stdout exactly:
//   static: java.lang.UnsatisfiedLinkError: 'int UnboundNativeProbe.unboundStatic(java.lang.Object)'
//   instance: java.lang.UnsatisfiedLinkError: 'java.lang.String UnboundNativeProbe.unboundInstance(int)'
//   reflect-static: ITE java.lang.UnsatisfiedLinkError: 'int UnboundNativeProbe.unboundStatic(java.lang.Object)'
//   reflect-instance: ITE java.lang.UnsatisfiedLinkError: 'java.lang.String UnboundNativeProbe.unboundInstance(int)'
public class UnboundNativeProbe {
    static native int unboundStatic(Object o);

    native String unboundInstance(int x);

    interface Call {
        Object run() throws Throwable;
    }

    static void report(String label, Call call) {
        try {
            call.run();
            System.out.println(label + ": no exception");
        } catch (java.lang.reflect.InvocationTargetException e) {
            Throwable t = e.getCause();
            System.out.println(label + ": ITE " + t.getClass().getName() + ": " + t.getMessage());
        } catch (Throwable t) {
            System.out.println(label + ": " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) {
        report("static", () -> unboundStatic("x"));
        report("instance", () -> new UnboundNativeProbe().unboundInstance(1));
        report(
                "reflect-static",
                () ->
                        UnboundNativeProbe.class
                                .getDeclaredMethod("unboundStatic", Object.class)
                                .invoke(null, "x"));
        report(
                "reflect-instance",
                () ->
                        UnboundNativeProbe.class
                                .getDeclaredMethod("unboundInstance", int.class)
                                .invoke(new UnboundNativeProbe(), 1));
    }
}
