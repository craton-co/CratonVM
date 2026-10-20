// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;

/**
 * zt t1/nd: a {@code Lookup.defineClass} / {@code defineHiddenClass} refused
 * on a SPENT heap must leave nothing defined, and this probe must reach the
 * refusal INSIDE the define native on CratonVM (ZGC round zt, wave t1, lane
 * nd; it replaces the non-discriminating {@code Zs8Nd7DefineRetry} rows). The
 * {@code ClassLoader.defineClass} twin is {@code Zt1NdDefineRetry}.
 *
 * <p>Unlike {@code ClassLoader.defineClass}, {@code Lookup.defineClass}
 * allocates in Java BEFORE its native ({@code bytes.clone()}, the class-file
 * name parse, the definer), so on a heap spent to the last hole it fails in
 * Java and never asks for the mirror. So each round keeps a SPARE block of a
 * size that cycles with the round ({@code spareStep} bytes apart, 64 sizes),
 * allocated before the heap is filled and dropped right before the define:
 * the define's Java allocations take the spare's room after the collection
 * their refusal runs, and for the sizes between "the Java side fits" and "the
 * Java side and the mirror fit" the first refusal is the mirror's, inside the
 * native. Whatever stage refuses, a VM that publishes nothing before the
 * refusal answers the retry with a class.
 *
 * <p>Modes: {@code lookup} defines {@code Zt1NdLk<i>} (this class's package
 * and loader, so every round's retry meets its own first attempt);
 * {@code hidden} defines hidden classes (a retry cannot collide: the row
 * counts failures other than {@code OutOfMemoryError}, and the census line
 * below is its evidence). On {@code OutOfMemoryError} a round frees
 * {@code freeChunks} 1 KiB chunks and retries once, then refills. Defined
 * classes stay live.
 *
 * <p>stdout (one line, deterministic): {@code mode=<m> linkage=<retries
 * answered by LinkageError> bad=<other failures> rounds=<n>}; HotSpot prints
 * {@code linkage=0 bad=0}. stderr: the refusal counts (they differ by VM). On
 * CratonVM, {@code CRATONVM_DBG_ZGC_DEFINE_MIRROR=1} prints
 * {@code [GC] zgc-define-mirror: reserved= refused= adopted=} at every refused
 * reservation ({@code refused>=1}: the native's refusal was reached).
 * Usage: {@code java -Xmx32m Zt1NdLookupDefineRetry [lookup|hidden] [rounds]
 * [freeChunks] [spareStep]} (defaults lookup, 128 = two cycles of the 64 spare sizes, 64, 32).
 */
public class Zt1NdLookupDefineRetry {
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

    /** The spare block the next define may use (dropped right before it). */
    static byte[] spare;

    /** Fill the heap, keeping a spare block of {@code spareBytes} first. */
    static void fill(ArrayList<Object> live, int spareBytes) {
        try {
            spare = new byte[spareBytes];
        } catch (OutOfMemoryError e) {
            spare = null;
        }
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

    static Class<?> def(MethodHandles.Lookup lookup, boolean hidden, byte[] b)
            throws IllegalAccessException {
        return hidden ? lookup.defineHiddenClass(b, false).lookupClass() : lookup.defineClass(b);
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "lookup";
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 128;
        int freeChunks = args.length > 2 ? Integer.parseInt(args[2]) : 64;
        int spareStep = args.length > 3 ? Integer.parseInt(args[3]) : 32;
        boolean hidden = mode.equals("hidden");
        if (!hidden && !mode.equals("lookup")) {
            throw new IllegalArgumentException("mode: lookup | hidden");
        }
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        String prefix = hidden ? "Zt1NdHid" : "Zt1NdLk";
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
        fill(live, 0);
        int oome = 0;
        int retryOome = 0;
        int linkage = 0;
        int bad = 0;
        int defined = 0;
        for (int i = 0; i < rounds; i++) {
            // This round's spare was kept by the previous fill; the next
            // round's size is chosen here.
            int nextSpare = ((i + 1) % 64) * spareStep;
            spare = null;
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
            } catch (Throwable t) {
                bad++;
            }
            fill(live, nextSpare);
        }
        live.clear();
        tail = null;
        spare = null;
        System.out.println("mode=" + mode + " linkage=" + linkage + " bad=" + bad
                + " rounds=" + rounds);
        System.err.println("zt1nd lookup-define-retry " + mode + ": oome=" + oome
                + " retry_oome=" + retryOome + " defined=" + defined);
    }
}
