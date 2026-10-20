// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L9 -- the NullPointerException Tomcat's
// org.apache.tomcat.util.buf.CharChunk comparison methods raise for a null
// String argument. Tomcat's bytecode does `s.length()` on the parameter `s`
// (its jars carry a LocalVariableTable), so HotSpot says
//   Cannot invoke "String.length()" because "s" is null
// CratonVM's native shadows of these methods (native-builtins lib.rs,
// `char_chunk_null_arg_npe`) appended " (CharChunk.<method>)" to that text
// (fixed in wave 98).
//
// SPECIAL SETUP: needs a Tomcat jar with org.apache.tomcat.util.buf.CharChunk
// on the class path (tomcat-embed-core-10.1.x / 11.x or tomcat-util), e.g.
//   $J/javac -d out tools/probes/interp/L9/L9W98TomcatCharChunkNpe.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp "out;tomcat-embed-core.jar" L9W98TomcatCharChunkNpe
//   $J/java -cp "out;tomcat-embed-core.jar" L9W98TomcatCharChunkNpe
// Without the jar it prints "no tomcat" (both VMs alike).
//
// HOTSPOT_EXPECTED_BEGIN (Tomcat 10.1 / 11 jars)
// equals: java.lang.NullPointerException: Cannot invoke "String.length()" because "s" is null
// equalsIgnoreCase: java.lang.NullPointerException: Cannot invoke "String.length()" because "s" is null
// startsWith: java.lang.NullPointerException: Cannot invoke "String.length()" because "s" is null
// startsWithIgnoreCase: java.lang.NullPointerException: Cannot invoke "String.length()" because "s" is null
// equals-no-buffer: false
// HOTSPOT_EXPECTED_END
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L9W98TomcatCharChunkNpe {
    public static void main(String[] args) throws Exception {
        Class<?> cc;
        try {
            cc = Class.forName("org.apache.tomcat.util.buf.CharChunk");
        } catch (ClassNotFoundException e) {
            System.out.println("no tomcat");
            return;
        }
        Object chunk = cc.getConstructor().newInstance();
        cc.getMethod("setChars", char[].class, int.class, int.class)
                .invoke(chunk, "hello".toCharArray(), 0, 5);
        call(cc, chunk, "equals", new Class<?>[] {String.class}, new Object[] {null});
        call(cc, chunk, "equalsIgnoreCase", new Class<?>[] {String.class}, new Object[] {null});
        call(cc, chunk, "startsWith", new Class<?>[] {String.class}, new Object[] {null});
        call(cc, chunk, "startsWithIgnoreCase", new Class<?>[] {String.class, int.class},
                new Object[] {null, 0});
        Object empty = cc.getConstructor().newInstance();
        Object r = cc.getMethod("equals", String.class).invoke(empty, (Object) null);
        System.out.println("equals-no-buffer: " + r);
    }

    static void call(Class<?> cc, Object chunk, String name, Class<?>[] types, Object[] args)
            throws Exception {
        Method m = cc.getMethod(name, types);
        try {
            System.out.println(name + ": ok " + m.invoke(chunk, args));
        } catch (InvocationTargetException e) {
            System.out.println(name + ": " + e.getCause());
        }
    }
}
