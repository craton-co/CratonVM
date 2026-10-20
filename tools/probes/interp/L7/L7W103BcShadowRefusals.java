// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L7 (i100-L7-native-shadow-npe-texts, BouncyCastle
// residue): our native shadows of BouncyCastle methods refused a bad offset / short array
// up front with "ArrayIndexOutOfBoundsException: Array index out of range: n" and changed
// nothing. HotSpot runs BC's bytecode: its own exception (or the JVM's "Index i out of
// bounds for length n" at the access, an arraycopy text, NegativeArraySizeException), after
// whatever partial writes BC made. The shadows now re-run BC's bytecode on such a refusal
// (`bc_register_rerun!`, `own_refusal_runs_shadowed_bytecode`).
// Expected output: HotSpot 25 with the same jar (the orchestrator records it); every row is
// `<label> -> <exception class>: <message>` plus, where BC wrote before throwing, the array
// contents after.
// Setup (needs the BC jar on both the compile and the run class path):
//   javac -cp ~/.m2/repository/org/bouncycastle/bcprov-jdk18on/1.78/bcprov-jdk18on-1.78.jar -d <dir> L7W103BcShadowRefusals.java
//   cratonvm --java-home <jdk25> -cp <dir>:<bcprov-1.78.jar> L7W103BcShadowRefusals   (both modes; repeat with 1.84)
import java.util.Arrays;

import org.bouncycastle.crypto.digests.SHA256Digest;
import org.bouncycastle.crypto.digests.WhirlpoolDigest;
import org.bouncycastle.crypto.prng.DigestRandomGenerator;
import org.bouncycastle.crypto.params.KeyParameter;
import org.bouncycastle.util.Pack;

public class L7W103BcShadowRefusals {

    interface Body {
        String run() throws Throwable;
    }

    static void row(String label, Body b) {
        try {
            System.out.println(label + " -> " + b.run());
        } catch (Throwable t) {
            System.out.println(label + " -> " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) {
            sb.append(String.format("%02x", x & 0xff));
        }
        return sb.toString();
    }

    public static void main(String[] args) {
        row("pack-le-short", () -> "" + Pack.littleEndianToInt(new byte[3], 0));
        row("pack-be-negative", () -> "" + Pack.bigEndianToInt(new byte[8], -1));
        row("pack-be-null", () -> "" + Pack.bigEndianToInt(null, 0));
        byte[] out = new byte[5];
        row("pack-int-array-partial", () -> {
            Pack.intToBigEndian(new int[] {0x01020304, 0x05060708}, out, 0);
            return "ok";
        });
        System.out.println("pack-int-array-partial bytes " + hex(out));
        int[] ns = new int[3];
        row("pack-to-int-array-partial", () -> {
            Pack.littleEndianToInt(new byte[] {1, 0, 0, 0, 2, 0, 0, 0, 3}, 0, ns);
            return "ok";
        });
        System.out.println("pack-to-int-array-partial ints " + Arrays.toString(ns));

        WhirlpoolDigest w = new WhirlpoolDigest();
        row("whirlpool-update-past-end", () -> {
            w.update(new byte[] {1, 2, 3, 4}, 2, 5);
            return "ok";
        });
        byte[] h = new byte[w.getDigestSize()];
        w.doFinal(h, 0);
        System.out.println("whirlpool-after " + hex(h).substring(0, 32));
        row("whirlpool-update-negative", () -> {
            new WhirlpoolDigest().update(new byte[4], -1, 2);
            return "ok";
        });

        row("key-parameter-past-end", () -> "" + new KeyParameter(new byte[4], 2, 5).getKey().length);
        row("key-parameter-negative-length", () -> "" + new KeyParameter(new byte[4], 0, -1).getKey().length);
        row("key-parameter-null", () -> "" + new KeyParameter(null).getKey().length);

        row("digest-random-negative-start", () -> {
            new DigestRandomGenerator(new SHA256Digest()).nextBytes(new byte[4], -1, 2);
            return "ok";
        });
        byte[] rnd = new byte[4];
        row("digest-random-past-end", () -> {
            DigestRandomGenerator g = new DigestRandomGenerator(new SHA256Digest());
            g.addSeedMaterial(42L);
            g.nextBytes(rnd, 2, 5);
            return "ok";
        });
        System.out.println("digest-random-past-end bytes " + hex(rnd));

        long[] z = new long[2];
        row("sect113-add-short-z", () -> {
            org.bouncycastle.math.ec.custom.sec.SecT113Field.add(new long[] {1, 2}, new long[] {4, 8}, new long[1]);
            return "ok";
        });
        row("sect113-add-ok", () -> {
            org.bouncycastle.math.ec.custom.sec.SecT113Field.add(new long[] {1, 2}, new long[] {4, 8}, z);
            return Arrays.toString(z);
        });
    }
}
