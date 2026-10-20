// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;

/**
 * zt t1/nd: a {@code ClassLoader.defineClass} refused on a SPENT heap must
 * leave nothing defined, and this probe must actually reach that refusal on
 * CratonVM (ZGC round zt, wave t1, lane nd; it replaces the non-discriminating
 * {@code Zs7Nd6DefineRetry} rows and the unfinished zs s10/nd9b edits).
 *
 * <p>HotSpot creates a class's mirror inside {@code define_instance_class}, so
 * an {@code OutOfMemoryError} from {@code defineClass} leaves the name
 * undefined and the program's retry (after it freed some memory) defines the
 * class. A VM that mints the mirror AFTER publishing the class answers that
 * retry with {@code LinkageError: duplicate class definition}.
 *
 * <p>Why {@code Zs7Nd6DefineRetry} never refused on CratonVM ({@code oome=0}
 * where HotSpot printed {@code oome=400}): it filled the heap with 1 KiB chunks
 * only, and CratonVM's free-list arena keeps holes below 1 KiB that a mirror
 * fits in (HotSpot's ZGC pages bump-allocate and keep none). This probe:
 * <ol>
 * <li>fills the heap with 1 KiB chunks, then with a descending ladder of
 * reference cells (a linked chain, no growing collection) down to the
 * smallest reference-holding object, so no hole a mirror fits in is left;</li>
 * <li>names its classes in the UNNAMED package, so {@code preDefineClass}'s
 * {@code checkCerts} takes no package substring before the native;</li>
 * <li>defines one warm class in EVERY round's loader first, so the loader's
 * lazily built state ({@code package2certs}, {@code packages}, HotSpot's
 * {@code ClassLoader.classes} list for {@code addClass}) allocates nothing on
 * the spent heap. The first allocation of a round's define is then the
 * mirror, inside the native, on both VMs.</li>
 * </ol>
 *
 * <p>stdout (one line, deterministic): {@code linkage=<retries answered by
 * LinkageError> bad=<other failures> rounds=<n>}; HotSpot prints
 * {@code linkage=0 bad=0}. stderr: the refusal counts (they differ by VM). On
 * CratonVM, {@code CRATONVM_DBG_ZGC_DEFINE_MIRROR=1} prints
 * {@code [GC] zgc-define-mirror: reserved= refused= adopted=} at every refused
 * reservation: {@code refused>=1} proves the reservation path engaged; under
 * {@code CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0} it prints nothing and stdout's
 * {@code linkage} goes non-zero (the kill-arm DIFF).
 * Usage: {@code java -Xmx32m Zt1NdDefineRetry [rounds] [freeChunks]}
 * (defaults 100 and 64; every OOME of a fill runs collections, so a round costs several).
 */
public class Zt1NdDefineRetry {
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

    /** The tail ladder's cell lengths (reference slots per {@code Object[]}). */
    static final int[] TAIL_CELLS = {16, 1};

    /** The tail: a chain of {@code Object[]} cells, slot 0 = the next cell. */
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
        Ldr[] loaders = new Ldr[rounds];
        String[] names = new String[rounds];
        byte[][] bytes = new byte[rounds][];
        Class<?>[] warmKept = new Class<?>[rounds];
        byte[] warmBytes = classBytes("Zt1NdWarm");
        for (int i = 0; i < rounds; i++) {
            loaders[i] = new Ldr();
            names[i] = "Zt1NdGen" + i;
            bytes[i] = classBytes("Zt1NdGen" + i);
            // Each loader's lazily built state, made with room to spare.
            warmKept[i] = loaders[i].def("Zt1NdWarm", warmBytes);
        }
        // Warm the define path (linkage, compilation) with room to spare.
        Ldr warm = new Ldr();
        for (int i = 0; i < 50; i++) {
            warm.def("Zt1NdW" + i, classBytes("Zt1NdW" + i));
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
        System.err.println("zt1nd define-retry: oome=" + oome + " retry_oome=" + retryOome
                + " defined=" + defined);
    }
}
