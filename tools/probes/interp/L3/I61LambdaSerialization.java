// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.io.*;
import java.lang.invoke.*;
public class I61LambdaSerialization {
    static int runs;
    static Runnable lambda(int value) { return (Runnable & Serializable) () -> runs += value; }
    public static void main(String[] args) throws Throwable {
        for (int i=0; i<2; i++) {
            Runnable r=lambda(3);
            Class<?> proxy=r.getClass();
            var lookup=MethodHandles.privateLookupIn(proxy,MethodHandles.lookup());
            var member=proxy.getDeclaredMethod("writeReplace"); member.setAccessible(true);
            MethodHandle own=lookup.unreflect(member);
            MethodType requested=MethodType.methodType(Object.class,Object.class);
            MethodHandle adapted=own.asType(requested);
            if (adapted==own || !adapted.type().equals(requested) || own.type().parameterType(0)!=proxy)
                throw new AssertionError("lost Class identity during adaptation");
            Object result=(Object)adapted.invokeExact((Object)r);
            if (!(result instanceof SerializedLambda) || !(member.invoke(r) instanceof SerializedLambda))
                throw new AssertionError("writeReplace result");
            var bytes=new ByteArrayOutputStream();
            try (var stream=new ObjectOutputStream(bytes)) { stream.writeObject(r); }
            try (var stream=new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
                ((Runnable)stream.readObject()).run();
            }
            System.out.println("round="+i+" runs="+runs);
            System.gc();
        }
    }
}
