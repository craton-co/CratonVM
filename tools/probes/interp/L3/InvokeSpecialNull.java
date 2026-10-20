// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, lane L3: `invokespecial` on a null receiver.
//
// `o.secret()` for a private method of the SAME class compiles to
// `invokespecial` (javac only switches to invokevirtual for calls from another
// nest member). JVMS 6.5 requires NullPointerException before the callee runs.
// Before the fix, CratonVM's cold `invokespecial` path had no null check at
// all: it pushed the callee frame with `this == null` and ran the body, so the
// "cold" lines below printed "ran with null this" instead of the NPE.
//
// HotSpot 25 prints (compiled WITHOUT -g; with -g the local is named "o"):
//
//   cold NPE Cannot invoke "InvokeSpecialNull.secret()" because "<local0>" is null
//   cold NPE Cannot invoke "InvokeSpecialNull.secret()" because "<local0>" is null
//   warm sum=420000
//   warm NPE Cannot invoke "InvokeSpecialNull.secret()" because "<local0>" is null
//   warm NPE Cannot invoke "InvokeSpecialNull.secret()" because "<local0>" is null
//
// The exception class is the part that must match; compare the message too.
public class InvokeSpecialNull {
    private int value = 42;

    private int secret() {
        return value;
    }

    static int call(InvokeSpecialNull o) {
        return o.secret();
    }

    static void tryNull(String phase) {
        try {
            int r = call(null);
            System.out.println(phase + " ran with null this (wrong): " + r);
        } catch (NullPointerException e) {
            System.out.println(phase + " NPE " + e.getMessage());
        }
    }

    public static void main(String[] args) {
        tryNull("cold");
        tryNull("cold");
        InvokeSpecialNull o = new InvokeSpecialNull();
        long sum = 0;
        for (int i = 0; i < 10_000; i++) {
            sum += call(o);
        }
        System.out.println("warm sum=" + sum);
        tryNull("warm");
        tryNull("warm");
    }
}
