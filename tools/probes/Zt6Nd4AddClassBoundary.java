// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;

/**
 * zt t6/nd4: {@code Zt1NdDefineRetry} with every round's loader at a growth
 * boundary of its {@code ClassLoader.classes} list (ZGC round zt, wave t6,
 * lane nd4; {@code zgc-zt1nd-define-and-door-residuals-after-the-merge-20261019.md}
 * item 1).
 *
 * <p>Each loader first defines {@code warm} classes (default 10: the empty
 * list's first array holds 10, so the next {@code addClass} must grow it to
 * 15; 15 and 22 are the next boundaries). Then the heap is spent and the
 * round's define runs. On HotSpot the growth inside {@code addClass} runs
 * before the class enters the dictionary, so its {@code OutOfMemoryError}
 * leaves the name undefined and the retry (after freeing memory) defines it:
 * {@code linkage=0}. A VM that grows the list after publishing the class
 * answers the retry with {@code LinkageError: duplicate class definition}.
 *
 * <p>stdout (one line, deterministic): {@code linkage=<n> bad=<n> rounds=<n>};
 * HotSpot prints {@code linkage=0 bad=0}. stderr: the refusal counts. Run it
 * in both modes: since zt t7/cl the real-JDK {@code defineClass1} door makes
 * the {@code addClass} upcall in both (before t7 it made none, so this row
 * read {@code linkage=0} on every arm). Kill control:
 * {@code CRATONVM_ZGC_ADDCLASS_PRESIZE=0} restores the old order, and
 * {@code linkage} goes non-zero if any round's refusal was the growth.
 * Usage: {@code java -Xmx32m Zt6Nd4AddClassBoundary [rounds] [freeChunks] [warm]}.
 */
public class Zt6Nd4AddClassBoundary {
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

    static final int[] TAIL_CELLS = {16, 1};

    static Object[] tail;

    static void fill(ArrayList<Object> live) {
        try {
            while (true) {
                live.add(new byte[1024 - 16]);
            }
        } catch (OutOfMemoryError e) {
            // full at 1 KiB granularity
        }
        for (int cells : TAIL_CELLS) {
            try {
                while (true) {
                    Object[] cell = new Object[cells];
                    cell[0] = tail;
                    tail = cell;
                }
            } catch (OutOfMemoryError e) {
                // full at this cell's granularity
            }
        }
    }

    static void free(ArrayList<Object> live, int chunks) {
        tail = null;
        for (int i = 0; i < chunks && !live.isEmpty(); i++) {
            live.remove(live.size() - 1);
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 100;
        int freeChunks = args.length > 1 ? Integer.parseInt(args[1]) : 64;
        int warmCount = args.length > 2 ? Integer.parseInt(args[2]) : 10;
        Ldr[] loaders = new Ldr[rounds];
        String[] names = new String[rounds];
        byte[][] bytes = new byte[rounds][];
        Class<?>[][] warmKept = new Class<?>[rounds][warmCount];
        byte[][] warmBytes = new byte[warmCount][];
        for (int w = 0; w < warmCount; w++) {
            warmBytes[w] = classBytes("Zt6Nd4Warm" + w);
        }
        for (int i = 0; i < rounds; i++) {
            loaders[i] = new Ldr();
            names[i] = "Zt6Nd4Gen" + i;
            bytes[i] = classBytes("Zt6Nd4Gen" + i);
            for (int w = 0; w < warmCount; w++) {
                warmKept[i][w] = loaders[i].def("Zt6Nd4Warm" + w, warmBytes[w]);
            }
        }
        Ldr warm = new Ldr();
        for (int i = 0; i < 50; i++) {
            warm.def("Zt6Nd4W" + i, classBytes("Zt6Nd4W" + i));
        }
        Class<?>[] kept = new Class<?>[rounds];
        ArrayList<Object> live = new ArrayList<>(1 << 16);
        System.gc();
        fill(live);
        int oome = 0;
        int retryOome = 0;
        int linkage = 0;
        int bad = 0;
        int defined = 0;
        for (int i = 0; i < rounds; i++) {
            try {
                kept[i] = loaders[i].def(names[i], bytes[i]);
                defined++;
            } catch (OutOfMemoryError e) {
                oome++;
                free(live, freeChunks);
                try {
                    kept[i] = loaders[i].def(names[i], bytes[i]);
                    defined++;
                } catch (LinkageError e2) {
                    linkage++;
                } catch (OutOfMemoryError e2) {
                    retryOome++;
                } catch (Throwable t) {
                    bad++;
                }
                fill(live);
            } catch (Throwable t) {
                bad++;
            }
        }
        live.clear();
        tail = null;
        System.out.println("linkage=" + linkage + " bad=" + bad + " rounds=" + rounds);
        System.err.println("zt6nd4 addclass-boundary: oome=" + oome + " retry_oome=" + retryOome
                + " defined=" + defined);
    }
}
