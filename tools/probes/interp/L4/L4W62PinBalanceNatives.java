// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L4 -- the natives the `[pin-balance]
// LEAK` census named (`docs/internal/fixed-bugs/interpreter-L4-natives-leave-pins-on-a-normal-return-FIXED-20261026.md`):
// `ClassLoader.defineClass0/1/2`, `FileSystemProvider.newByteChannel`,
// `Constructor.newInstance`, `ByteArrayOutputStream()` and
// `DataOutputStream(OutputStream)`. Each row drives one of them on its normal
// return, first and again after a warm loop. The Java-visible answers are
// HotSpot's; the fix itself is seen on a DEBUG build, which prints no
// `[pin-balance] LEAK` line for these callees (run with
// `CRATONVM_ENABLE_NATIVE_RING=1` to have the census name them).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// baos: 0 3
// dos: 4 4
// ctor: [x]
// bind: hello
// hidden: true
// define1: Def1 true
// define2: Def2 true
// channel: 5
// warm: 2000
// baos-again: 0 3
// ctor-again: [y]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W62PinBalanceNatives`.
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.nio.ByteBuffer;
import java.nio.channels.SeekableByteChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.List;

public class L4W62PinBalanceNatives {
    public static class Box {
        final String v;

        public Box(String v) {
            this.v = v;
        }

        public String hello() {
            return "hello";
        }

        @Override
        public String toString() {
            return "[" + v + "]";
        }
    }

    /** Defines a class from bytes through `defineClass` (byte[] and ByteBuffer forms). */
    static final class Definer extends ClassLoader {
        Definer() {
            super(L4W62PinBalanceNatives.class.getClassLoader());
        }

        Class<?> fromArray(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }

        Class<?> fromBuffer(String name, byte[] b) {
            return defineClass(name, ByteBuffer.wrap(b), null);
        }
    }

    /** A minimal class file: `public class <name> extends Object {}` (version 52). */
    static byte[] emptyClass(String name) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(5); // constant pool count
        out.writeByte(7); // #1 Class #2
        out.writeShort(2);
        out.writeByte(1); // #2 Utf8 name
        out.writeUTF(name);
        out.writeByte(7); // #3 Class #4
        out.writeShort(4);
        out.writeByte(1); // #4 Utf8 java/lang/Object
        out.writeUTF("java/lang/Object");
        out.writeShort(0x0021); // public super
        out.writeShort(1);
        out.writeShort(3);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(0); // methods
        out.writeShort(0); // attributes
        out.flush();
        return bytes.toByteArray();
    }

    static String baos() {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        int before = b.size();
        b.write(1);
        b.write(2);
        b.write(3);
        return before + " " + b.size();
    }

    static String ctor(String v) throws Exception {
        return Box.class.getConstructor(String.class).newInstance(v).toString();
    }

    public static void main(String[] args) throws Throwable {
        System.out.println("baos: " + baos());
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        DataOutputStream dos = new DataOutputStream(sink);
        dos.writeInt(7);
        System.out.println("dos: " + dos.size() + " " + sink.size());
        System.out.println("ctor: " + ctor("x"));
        MethodHandle hello = MethodHandles.lookup().bind(new Box("b"), "hello", MethodType.methodType(String.class));
        System.out.println("bind: " + (String) hello.invokeExact());
        MethodHandles.Lookup hidden = MethodHandles.lookup().defineHiddenClass(
                emptyClass("L4W62PinBalanceNatives$Hid"), false);
        System.out.println("hidden: " + hidden.lookupClass().isHidden());
        Definer d = new Definer();
        Class<?> c1 = d.fromArray("Def1", emptyClass("Def1"));
        System.out.println("define1: " + c1.getName() + " " + (c1.getClassLoader() == d));
        Class<?> c2 = d.fromBuffer("Def2", emptyClass("Def2"));
        System.out.println("define2: " + c2.getName() + " " + (c2.getClassLoader() == d));
        Path tmp = Files.createTempFile("l4w62", ".bin");
        try {
            Files.write(tmp, new byte[] {1, 2, 3, 4, 5});
            try (SeekableByteChannel ch = Files.newByteChannel(tmp, StandardOpenOption.READ)) {
                System.out.println("channel: " + ch.size());
            }
        } finally {
            Files.deleteIfExists(tmp);
        }
        long n = 0;
        List<Object> keep = new ArrayList<>();
        for (int i = 0; i < 1000; i++) {
            n += baos().length() > 0 ? 1 : 0;
            keep.add(new DataOutputStream(new ByteArrayOutputStream()));
            n += ctor("w").length() > 0 ? 1 : 0;
            if (keep.size() > 64) {
                keep.clear();
            }
        }
        System.out.println("warm: " + n);
        System.out.println("baos-again: " + baos());
        System.out.println("ctor-again: " + ctor("y"));
    }
}
