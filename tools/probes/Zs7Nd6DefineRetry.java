// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;

/**
 * zs s7/nd6: a define refused on a FULL heap must leave nothing defined
 * (ZGC round zs, wave s7, lane nd6;
 * {@code zgc-zs2nd2-proposal-mirror-inside-the-define-20261014.md}).
 *
 * <p>HotSpot creates a class's mirror inside {@code define_instance_class}, so
 * an {@code OutOfMemoryError} from {@code ClassLoader.defineClass} leaves the
 * name undefined and the program's retry (after it freed some memory) defines
 * the class. A VM that mints the mirror AFTER publishing the class answers
 * that retry with {@code LinkageError: duplicate class definition}.
 *
 * <p>Everything a round needs (one loader per round, the names, the class
 * bytes) is made before the heap is filled with live 1 KiB chunks until
 * {@code OutOfMemoryError}. Each round defines one named class in its own
 * loader; on {@code OutOfMemoryError} it frees {@code freeChunks} chunks and
 * retries once in the same loader, then refills. Defined classes stay live.
 *
 * <p>stdout (one line, deterministic): {@code linkage=<retries answered by
 * LinkageError> bad=<other failures> rounds=<n>}; HotSpot prints
 * {@code linkage=0 bad=0}. stderr: the refusal counts (they differ by VM).
 * Usage: {@code java -Xmx32m Zs7Nd6DefineRetry [rounds] [freeChunks]}
 * (defaults 400 and 64).
 *
 * <p>zt t1/nd: NON-DISCRIMINATING on CratonVM (host s7a: {@code oome=0} where
 * HotSpot refuses every round), so stdout is SAME without the refusal being
 * reached. Kept unchanged as a row; the discriminating twin is {@code Zt1NdDefineRetry}.
 */
public class Zs7Nd6DefineRetry {
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

    static void fill(ArrayList<Object> live) {
        try {
            while (true) {
                live.add(new byte[1024 - 16]);
            }
        } catch (OutOfMemoryError e) {
            // full
        }
    }

    static void free(ArrayList<Object> live, int chunks) {
        for (int i = 0; i < chunks && !live.isEmpty(); i++) {
            live.remove(live.size() - 1);
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        int freeChunks = args.length > 1 ? Integer.parseInt(args[1]) : 64;
        Ldr[] loaders = new Ldr[rounds];
        String[] names = new String[rounds];
        byte[][] bytes = new byte[rounds][];
        for (int i = 0; i < rounds; i++) {
            loaders[i] = new Ldr();
            names[i] = "zs7nd6.Gen" + i;
            bytes[i] = classBytes("zs7nd6/Gen" + i);
        }
        // Warm the define path (linkage, compilation) with room to spare.
        Ldr warm = new Ldr();
        for (int i = 0; i < 50; i++) {
            warm.def("zs7nd6.Warm" + i, classBytes("zs7nd6/Warm" + i));
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
                }
                fill(live);
            } catch (Throwable t) {
                bad++;
            }
        }
        live.clear();
        System.out.println("linkage=" + linkage + " bad=" + bad + " rounds=" + rounds);
        System.err.println("zs7nd6 define-retry: oome=" + oome + " retry_oome=" + retryOome
                + " defined=" + defined);
    }
}
