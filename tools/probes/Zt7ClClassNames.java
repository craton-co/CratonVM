// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryMXBean;

/**
 * zt t7/cl: class-name parity of classes a loader object defines (ZGC round
 * zt, wave t7, lane cl;
 * {@code zgc-zs4nd4-unloaded-class-names-stay-in-the-strong-string-pool-20261015.md}
 * residual 1).
 *
 * <p>Part 1 (stdout, deterministic): HotSpot's {@code Class.getName()} answers
 * the interned binary name, so it is {@code ==} to the same literal, for a
 * class in the unnamed package and for a packaged one, and it is its own
 * {@code intern()}. CratonVM keeps that answer whether or not the mirror's
 * internal-name String is pooled ({@code CRATONVM_ZGC_CLASS_NAME_UNPOOLED}).
 *
 * <p>Part 2: {@code rounds} rounds, each a fresh loader that defines
 * {@code perRound} classes (their names never asked) and is dropped, then
 * {@code System.gc()}. stdout: {@code rounds=<r> defined=<n>}. stderr: the
 * used heap after each round's collection ({@code used_kb=}) and the slope
 * ({@code growth_bytes_per_define=}); a pooled name per define shows as ~84
 * bytes per define on ZGC with the strong pool.
 *
 * <p>Usage: {@code java -XX:+UseZGC Zt7ClClassNames [rounds] [perRound]}.
 */
public class Zt7ClClassNames {
    static final class Ldr extends ClassLoader {
        Ldr() {
            super(null);
        }

        Class<?> def(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /** {@code public class <name> extends java.lang.Object {}} (version 52). */
    static byte[] classBytes(String internalName) throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bos);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(5); // cp 1..4
        out.writeByte(7);
        out.writeShort(2); // #1 Class #2
        out.writeByte(1);
        out.writeUTF(internalName); // #2
        out.writeByte(7);
        out.writeShort(4); // #3 Class #4
        out.writeByte(1);
        out.writeUTF("java/lang/Object"); // #4
        out.writeShort(0x0021);
        out.writeShort(1);
        out.writeShort(3);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(0); // methods
        out.writeShort(0); // attributes
        out.flush();
        return bos.toByteArray();
    }

    static long usedAfterGc(MemoryMXBean memory) {
        System.gc();
        return memory.getHeapMemoryUsage().getUsed();
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int perRound = args.length > 1 ? Integer.parseInt(args[1]) : 250;

        Ldr names = new Ldr();
        Class<?> unnamed = names.def("Zt7ClUnnamed", classBytes("Zt7ClUnnamed"));
        Class<?> packaged = names.def("zt7cl.Packaged", classBytes("zt7cl/Packaged"));
        System.out.println("unnamed getName==literal " + (unnamed.getName() == "Zt7ClUnnamed"));
        System.out.println("packaged getName==literal " + (packaged.getName() == "zt7cl.Packaged"));
        System.out.println("getName stable " + (packaged.getName() == packaged.getName()));
        System.out.println("getName interned " + (packaged.getName().intern() == packaged.getName()));
        String internal = new String("zt7cl/Packaged").intern();
        System.out.println("internal text is its own intern " + (internal == "zt7cl/Packaged"));

        MemoryMXBean memory = ManagementFactory.getMemoryMXBean();
        byte[][] bytes = new byte[perRound][];
        String[] binaryNames = new String[perRound];
        for (int i = 0; i < perRound; i++) {
            bytes[i] = classBytes("zt7cl/Gen" + i);
            binaryNames[i] = "zt7cl.Gen" + i;
        }
        long first = usedAfterGc(memory);
        long last = first;
        int defined = 0;
        for (int r = 0; r < rounds; r++) {
            Ldr loader = new Ldr();
            for (int i = 0; i < perRound; i++) {
                loader.def(binaryNames[i], bytes[i]);
                defined++;
            }
            loader = null;
            last = usedAfterGc(memory);
            System.err.println("round=" + r + " used_kb=" + (last / 1024));
        }
        System.err.println("growth_bytes_per_define=" + ((last - first) / Math.max(1, defined)));
        System.out.println("rounds=" + rounds + " defined=" + defined);
    }
}
