// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L4 (`CRATONVM_MH_COMPAT_ARRAY_ACCESSORS_JDK`,
// default off; step 3 of
// docs/internal/proposals/interpreter/i44-L4-proposal-invoke-a-jdk-delegating-handle-through-its-target-20261008.md
// and the `METHODHANDLES ARRAY ACCESSORS` pin of i43-L4): the three array
// accessor factories, their types, answers, refusals and `toString`, and a
// record round trip through serialization (`ObjectStreamClass$RecordSupport`
// builds its extractors with `arrayElementGetter(byte[].class)`). Every row
// must be HotSpot's under `--compatible` with the switch off (the pinned
// bridges) and on (the JDK bodies), and under `--jdk-only`.
//
// Run: javac -d . L4W118ArrayAccessorFactories.java
//      java -cp . L4W118ArrayAccessorFactories                          (HotSpot)
//      cratonvm --compatible [--nojit] -cp . L4W118ArrayAccessorFactories
//      CRATONVM_MH_COMPAT_ARRAY_ACCESSORS_JDK=1 cratonvm --compatible [--nojit] -cp . L4W118ArrayAccessorFactories
//      cratonvm --jdk-only -cp . L4W118ArrayAccessorFactories
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// types (int[],int)int (int[],int,int)void (int[])int (String[],int)String (Object[],int,Object)void (byte[],int)byte
// int 20 len 3
// ref b invoke 3
// byte 8
// oob Index 5 out of bounds for length 3
// ase java.lang.ArrayStoreException: java.lang.Integer
// npe java.lang.NullPointerException
// not-array not an array: class java.lang.String
// warm 33000
// record Point[x=3, b=4, l=5, s=p]
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W118ArrayAccessorFactories {
    record Point(int x, byte b, long l, String s) implements Serializable {}

    public static void main(String[] args) throws Throwable {
        MethodHandle gi = MethodHandles.arrayElementGetter(int[].class);
        MethodHandle si = MethodHandles.arrayElementSetter(int[].class);
        MethodHandle li = MethodHandles.arrayLength(int[].class);
        MethodHandle gs = MethodHandles.arrayElementGetter(String[].class);
        MethodHandle ss = MethodHandles.arrayElementSetter(Object[].class);
        MethodHandle gb = MethodHandles.arrayElementGetter(byte[].class);
        System.out.println("types " + gi.type() + " " + si.type() + " " + li.type() + " " + gs.type()
                + " " + ss.type() + " " + gb.type());
        int[] ia = {1, 2, 3};
        si.invokeExact(ia, 1, 20);
        System.out.println("int " + (int) gi.invokeExact(ia, 1) + " len " + (int) li.invokeExact(ia));
        String[] sa = {"a", "b"};
        System.out.println("ref " + (String) gs.invokeExact(sa, 1) + " invoke " + gi.invoke(ia, 2));
        System.out.println("byte " + (byte) gb.invokeExact(new byte[] {7, 8}, 1));
        try {
            int v = (int) gi.invokeExact(ia, 5);
            System.out.println("oob none " + v);
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("oob " + e.getMessage());
        }
        try {
            ss.invokeExact((Object[]) sa, 0, (Object) Integer.valueOf(1));
            System.out.println("ase none");
        } catch (ArrayStoreException e) {
            System.out.println("ase " + e.getClass().getName() + ": " + e.getMessage());
        }
        try {
            int n = (int) li.invokeExact((int[]) null);
            System.out.println("npe none " + n);
        } catch (NullPointerException e) {
            System.out.println("npe " + e.getClass().getName());
        }
        try {
            MethodHandles.arrayElementGetter(String.class);
            System.out.println("not-array none");
        } catch (IllegalArgumentException e) {
            System.out.println("not-array " + e.getMessage());
        }
        long warm = 0;
        for (int i = 0; i < 3000; i++) {
            warm += (int) gi.invokeExact(ia, i % 3) + (int) li.invokeExact(ia);
        }
        System.out.println("warm " + warm);

        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream oos = new ObjectOutputStream(bos)) {
            oos.writeObject(new Point(3, (byte) 4, 5L, "p"));
        }
        try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
            System.out.println("record " + ois.readObject());
        }
    }
}
