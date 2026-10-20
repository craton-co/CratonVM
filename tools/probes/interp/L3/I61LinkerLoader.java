// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// Compile this together with i61-loader/{Box,Caller}.java. The caller delegates
// Box to a user parent while another loader defines an equal-named Box. Linker
// types must use the initiating loader's class, in both modes and both tiers.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
public class I61LinkerLoader {
    static byte[] bytes(String name) throws Exception {
        try (var stream = I61LinkerLoader.class.getResourceAsStream("/"+name+".class")) { return stream.readAllBytes(); }
    }
    static class Parent extends ClassLoader {
        final byte[] box;
        Parent(String name, byte[] box) { super(name,null); this.box=box; }
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.equals("Box")) return defineClass(name,box,0,box.length);
            throw new ClassNotFoundException(name);
        }
    }
    static class Child extends ClassLoader {
        final byte[] caller;
        Child(Parent parent, byte[] caller) { super("caller",parent); this.caller=caller; }
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.equals("Caller")) return defineClass(name,caller,0,caller.length);
            throw new ClassNotFoundException(name);
        }
    }
    public static void main(String[] args) throws Throwable {
        byte[] box=bytes("Box"),caller=bytes("Caller");
        var left=new Parent("left",box);var right=new Parent("right",box);
        Class<?> other=Class.forName("Box",true,right);
        MethodHandle foreign=MethodHandles.publicLookup().unreflect(other.getMethod("read",other));
        MethodHandle foreignReturn=MethodHandles.publicLookup().unreflect(other.getMethod("self"));
        var child=new Child(left,caller);
        Class<?> owner=Class.forName("Caller",true,child);
        var run=owner.getMethod("run",MethodHandle.class,MethodHandle.class);
        System.out.println(run.invoke(null,foreign,foreignReturn));
        System.gc();
        System.out.println(run.invoke(null,foreign,foreignReturn));
    }
}
