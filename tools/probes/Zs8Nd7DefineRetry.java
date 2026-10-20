// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;

/**
 * zs s8/nd7: a {@code Lookup.defineClass} / {@code defineHiddenClass} refused
 * on a FULL heap must leave nothing defined (ZGC round zs, wave s8, lane nd7;
 * {@code zgc-zs7nd6-native-door-residuals-FIXED-20261019.md} §1). The
 * {@code ClassLoader.defineClass} twin is {@code Zs7Nd6DefineRetry}.
 *
 * <p>HotSpot creates a class's mirror inside {@code define_instance_class}
 * ({@code JVM_LookupDefineClass} included), so an {@code OutOfMemoryError} from
 * {@code Lookup.defineClass} leaves the name undefined and the program's retry
 * (after it freed some memory) defines the class. A VM that mints the mirror
 * AFTER publishing the class answers that retry with
 * {@code LinkageError: duplicate class definition}.
 *
 * <p>Modes: {@code lookup} defines {@code Zs8Nd7Gen<i>} (this class's package
 * and loader, so every round's retry meets its own first attempt);
 * {@code hidden} defines hidden classes (a retry cannot collide: the row
 * counts failures other than {@code OutOfMemoryError}). Everything a round
 * needs is made before the heap is filled with live 1 KiB chunks until
 * {@code OutOfMemoryError}; on {@code OutOfMemoryError} a round frees
 * {@code freeChunks} chunks and retries once, then refills. Defined classes
 * stay live.
 *
 * <p>stdout (one line, deterministic): {@code mode=<m> linkage=<retries
 * answered by LinkageError> bad=<other failures> rounds=<n>}; HotSpot prints
 * {@code linkage=0 bad=0}. stderr: the refusal counts (they differ by VM).
 * Usage: {@code java -Xmx32m Zs8Nd7DefineRetry [lookup|hidden] [rounds]
 * [freeChunks]} (defaults lookup, 400, 64).
 *
 * <p>zt t1/nd: NON-DISCRIMINATING on CratonVM (host s7a: {@code oome=0} where
 * HotSpot refuses every round), so stdout is SAME without the refusal being
 * reached. Kept unchanged as a row; the discriminating twin is {@code Zt1NdLookupDefineRetry}.
 */
public class Zs8Nd7DefineRetry {
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

    static Class<?> def(MethodHandles.Lookup lookup, boolean hidden, byte[] b)
            throws IllegalAccessException {
        return hidden ? lookup.defineHiddenClass(b, false).lookupClass() : lookup.defineClass(b);
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "lookup";
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        int freeChunks = args.length > 2 ? Integer.parseInt(args[2]) : 64;
        boolean hidden = mode.equals("hidden");
        if (!hidden && !mode.equals("lookup")) {
            throw new IllegalArgumentException("mode: lookup | hidden");
        }
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        String prefix = hidden ? "Zs8Nd7Hid" : "Zs8Nd7Gen";
        byte[][] bytes = new byte[rounds][];
        for (int i = 0; i < rounds; i++) {
            bytes[i] = classBytes(prefix + i);
        }
        // Warm the define path (linkage, compilation) with room to spare.
        for (int i = 0; i < 50; i++) {
            def(lookup, hidden, classBytes(prefix + "Warm" + i));
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
                kept[i] = def(lookup, hidden, bytes[i]);
                defined++;
            } catch (OutOfMemoryError e) {
                oome++;
                free(live, freeChunks);
                try {
                    kept[i] = def(lookup, hidden, bytes[i]);
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
        System.out.println("mode=" + mode + " linkage=" + linkage + " bad=" + bad
                + " rounds=" + rounds);
        System.err.println("zs8nd7 define-retry " + mode + ": oome=" + oome
                + " retry_oome=" + retryOome + " defined=" + defined);
    }
}
