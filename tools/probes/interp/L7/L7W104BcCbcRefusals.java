// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L7 (i100-L7-native-shadow-npe-texts, CBC residue): the
// native shadow of BouncyCastle's CBCBlockCipher.processBlock refused a negative offset with
// "ArrayIndexOutOfBoundsException: Array index out of range: n", a null array with its own NPE,
// and a missing cipher / IV with IllegalStateException texts of its own. HotSpot runs BC's
// bytecode: the length check first ("input buffer too short"), then the first array access
// (`Index -1 out of bounds for length 16`), JEP 358 texts for nulls. The shadow now finds each
// such case before it writes anything and runs the bytecode instead.
// Expected output: HotSpot 25 with the same jar (the orchestrator records it); each row is
// `<label> -> <result or exception class: message>`.
// Setup (needs the BC jar on both the compile and the run class path):
//   javac -cp ~/.m2/repository/org/bouncycastle/bcprov-jdk18on/1.78/bcprov-jdk18on-1.78.jar -d <dir> L7W104BcCbcRefusals.java
//   cratonvm --java-home <jdk25> -cp <dir>:<bcprov-1.78.jar> L7W104BcCbcRefusals   (both modes; repeat with 1.84)
import org.bouncycastle.crypto.BlockCipher;
import org.bouncycastle.crypto.engines.AESEngine;
import org.bouncycastle.crypto.engines.DESedeEngine;
import org.bouncycastle.crypto.modes.CBCBlockCipher;
import org.bouncycastle.crypto.params.KeyParameter;
import org.bouncycastle.crypto.params.ParametersWithIV;

public class L7W104BcCbcRefusals {

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

    @SuppressWarnings("deprecation")
    static CBCBlockCipher cbc(BlockCipher engine, boolean encrypt, int keyLen) {
        CBCBlockCipher c = new CBCBlockCipher(engine);
        c.init(encrypt, new ParametersWithIV(new KeyParameter(new byte[keyLen]), new byte[c.getBlockSize()]));
        return c;
    }

    @SuppressWarnings("deprecation")
    public static void main(String[] args) {
        row("aes-enc-ok", () -> "" + cbc(new AESEngine(), true, 16).processBlock(new byte[16], 0, new byte[16], 0));
        row("aes-enc-negative-in", () -> "" + cbc(new AESEngine(), true, 16).processBlock(new byte[16], -1, new byte[16], 0));
        row("aes-enc-negative-out", () -> "" + cbc(new AESEngine(), true, 16).processBlock(new byte[16], 0, new byte[16], -2));
        row("aes-enc-null-in", () -> "" + cbc(new AESEngine(), true, 16).processBlock(null, 0, new byte[16], 0));
        row("aes-dec-null-out", () -> "" + cbc(new AESEngine(), false, 16).processBlock(new byte[16], 0, null, 0));
        row("aes-dec-negative-out", () -> "" + cbc(new AESEngine(), false, 16).processBlock(new byte[16], 0, new byte[16], -3));
        row("des-enc-negative-out", () -> "" + cbc(new DESedeEngine(), true, 24).processBlock(new byte[8], 0, new byte[8], -3));
        row("des-dec-negative-in", () -> "" + cbc(new DESedeEngine(), false, 24).processBlock(new byte[8], -1, new byte[8], 0));
        row("aes-not-initialised", () -> "" + new CBCBlockCipher(new AESEngine()).processBlock(new byte[16], 0, new byte[16], 0));
    }
}
