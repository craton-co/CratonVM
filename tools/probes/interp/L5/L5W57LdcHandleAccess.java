// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L5 -- `ldc` of a `CONSTANT_MethodHandle`
// checks access from the referencing class (JVMS 5.4.3.5 / 5.4.4) in both
// modes: item 9 of
// `interpreter-L5-compatible-gates-that-still-differ-after-the-alignment-table-FIXED-20261007`.
// javac never emits such an `ldc`, so the probe writes the referencing class
// itself (`L5W57LdcGen`, defined by `Lookup.defineClass` into the probe's
// package) with two methods:
//
//   priv-*  `get()`: `ldc MethodHandle REF_invokeStatic Holder.secret()I`
//           (private to `L5W57LdcHandleAccess$Holder`, not a nestmate)
//   open-*  `getOpen()`: the same for the public `Holder.open()I`; the
//           handle is invoked
//
// `priv` is asked first and again last (a resolution error is the same error
// on every later attempt, JVMS 5.4.3).
//
// Run (no setup):
//   javac -d out L5W57LdcHandleAccess.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W57LdcHandleAccess
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   priv-1=java.lang.IllegalAccessError msg=class L5W57LdcGen tried to access private method 'int L5W57LdcHandleAccess$Holder.secret()' (L5W57LdcGen and L5W57LdcHandleAccess$Holder are in unnamed module of loader 'app')
//   open-1=7
//   priv-2=java.lang.IllegalAccessError msg=class L5W57LdcGen tried to access private method 'int L5W57LdcHandleAccess$Holder.secret()' (L5W57LdcGen and L5W57LdcHandleAccess$Holder are in unnamed module of loader 'app')
//   open-2=7

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Method;

public class L5W57LdcHandleAccess {
    public static final class Holder {
        private static int secret() {
            return 42;
        }

        public static int open() {
            return 7;
        }
    }

    static byte[] generator() throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(20); // constant pool count: #1..#19
        out.writeByte(7);
        out.writeShort(2); // #1 Class L5W57LdcGen
        out.writeByte(1);
        out.writeUTF("L5W57LdcGen"); // #2
        out.writeByte(7);
        out.writeShort(4); // #3 Class Object
        out.writeByte(1);
        out.writeUTF("java/lang/Object"); // #4
        out.writeByte(7);
        out.writeShort(6); // #5 Class Holder
        out.writeByte(1);
        out.writeUTF("L5W57LdcHandleAccess$Holder"); // #6
        out.writeByte(1);
        out.writeUTF("secret"); // #7
        out.writeByte(1);
        out.writeUTF("()I"); // #8
        out.writeByte(12);
        out.writeShort(7);
        out.writeShort(8); // #9 NameAndType secret ()I
        out.writeByte(10);
        out.writeShort(5);
        out.writeShort(9); // #10 Methodref Holder.secret
        out.writeByte(15);
        out.writeByte(6);
        out.writeShort(10); // #11 MethodHandle REF_invokeStatic #10
        out.writeByte(1);
        out.writeUTF("get"); // #12
        out.writeByte(1);
        out.writeUTF("()Ljava/lang/Object;"); // #13
        out.writeByte(1);
        out.writeUTF("Code"); // #14
        out.writeByte(1);
        out.writeUTF("open"); // #15
        out.writeByte(12);
        out.writeShort(15);
        out.writeShort(8); // #16 NameAndType open ()I
        out.writeByte(10);
        out.writeShort(5);
        out.writeShort(16); // #17 Methodref Holder.open
        out.writeByte(15);
        out.writeByte(6);
        out.writeShort(17); // #18 MethodHandle REF_invokeStatic #17
        out.writeByte(1);
        out.writeUTF("getOpen"); // #19
        out.writeShort(0x21); // public super
        out.writeShort(1);
        out.writeShort(3);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(2); // methods
        for (int[] m : new int[][] {{12, 11}, {19, 18}}) {
            out.writeShort(0x0009); // public static
            out.writeShort(m[0]);
            out.writeShort(13);
            out.writeShort(1); // attributes
            out.writeShort(14); // Code
            out.writeInt(2 + 2 + 4 + 3 + 2 + 2);
            out.writeShort(1); // max_stack
            out.writeShort(0); // max_locals
            out.writeInt(3);
            out.writeByte(0x12); // ldc
            out.writeByte(m[1]);
            out.writeByte(0xB0); // areturn
            out.writeShort(0); // exception table
            out.writeShort(0); // attributes
        }
        out.writeShort(0); // class attributes
        out.flush();
        return bytes.toByteArray();
    }

    interface Call {
        Object call() throws Throwable;
    }

    static String attempt(Call c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            if (t instanceof java.lang.reflect.InvocationTargetException && t.getCause() != null) {
                t = t.getCause();
            }
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> gen = MethodHandles.lookup().defineClass(generator());
        Method get = gen.getMethod("get");
        Method getOpen = gen.getMethod("getOpen");
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("priv-" + tag + "=" + attempt(() -> get.invoke(null)));
            System.out.println("open-" + tag + "=" + attempt(() -> (int) ((MethodHandle) getOpen.invoke(null)).invokeExact()));
        }
    }
}
