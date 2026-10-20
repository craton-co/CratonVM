// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 100, lane L5 -- `ClassLoader.defineClass(String,
// ByteBuffer, ProtectionDomain)` with a buffer whose class bytes do NOT start
// at position 0. The JDK body passes `b.position()` as `defineClass2`'s
// offset, and HotSpot reads at the direct buffer's address + offset; the VM's
// decoder (`lang_system::read_byte_buffer_define_class_slice`) added the
// position a second time. Heap buffers take the JDK's `byte[]` route
// (`defineClass1`) and are the control.
//
// Run (no setup):
//   javac -d out L5W100DefineClassBufferPosition.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W100DefineClassBufferPosition
//
// Expected HotSpot 25 output:
//   direct-pos0=w100b.A pos=0
//   direct-pos7=w100b.B pos=7
//   direct-slice=w100b.C pos=0
//   heap-pos7=w100b.D pos=7
//   direct-pos7-garbage=java.lang.ClassFormatError: Incompatible magic value 16909060 in class file w100b/E
// (The JDK body leaves the position where it was. The garbage row is
// HotSpot's header text, as `L5W42DefineClassErrors`' `buffer-garbage` row; the
// `--compatible` spelling follows that probe's rules.)

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.nio.ByteBuffer;

public class L5W100DefineClassBufferPosition {
    static byte[] cls(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object));
    }

    static final class L extends ClassLoader {
        L() {
            super(null);
        }

        Class<?> def(String name, ByteBuffer bb) {
            return defineClass(name, bb, null);
        }
    }

    static String pos(ByteBuffer bb) {
        return String.valueOf(bb.position());
    }

    static ByteBuffer at(boolean direct, int skip, byte[] body) {
        ByteBuffer bb = direct ? ByteBuffer.allocateDirect(skip + body.length + 5)
                : ByteBuffer.allocate(skip + body.length + 5);
        for (int i = 0; i < skip; i++) {
            bb.put((byte) 0x55);
        }
        bb.put(body);
        bb.put(new byte[5]);
        bb.position(skip);
        bb.limit(skip + body.length);
        return bb;
    }

    static void row(String label, java.util.concurrent.Callable<String> c) {
        String out;
        try {
            out = c.call();
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + "=" + out);
    }

    public static void main(String[] args) {
        L l = new L();
        row("direct-pos0", () -> {
            ByteBuffer bb = at(true, 0, cls("w100b.A"));
            return l.def("w100b.A", bb).getName() + " pos=" + pos(bb);
        });
        row("direct-pos7", () -> {
            ByteBuffer bb = at(true, 7, cls("w100b.B"));
            return l.def("w100b.B", bb).getName() + " pos=" + pos(bb);
        });
        row("direct-slice", () -> {
            ByteBuffer outer = at(true, 9, cls("w100b.C"));
            ByteBuffer bb = outer.slice();
            return l.def("w100b.C", bb).getName() + " pos=" + pos(bb);
        });
        row("heap-pos7", () -> {
            ByteBuffer bb = at(false, 7, cls("w100b.D"));
            return l.def("w100b.D", bb).getName() + " pos=" + pos(bb);
        });
        row("direct-pos7-garbage", () -> {
            byte[] junk = {1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0};
            ByteBuffer bb = at(true, 7, junk);
            return l.def("w100b.E", bb).getName();
        });
    }
}
