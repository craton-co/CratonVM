// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L7: the residue of
// i100-L7-native-shadow-npe-texts on the paths a real-JDK boot runs.
// RandomAccessFile on a closed descriptor raises io_util.c's / RandomAccessFile.c's
// IOException("Stream Closed") (native-io readBytes0 / read0 / length0 /
// getFilePointer / seek0 / write0), a zero-length read of a closed file
// answers 0 before the descriptor is looked at, and Mac.update on an
// uninitialized Mac refuses "MAC not initialized" before its range check
// (JDK bytecode; the synthetic Bridge in ssl_security.rs checks the range
// first but does not run when the JDK's Mac class is present).
// Expected HotSpot 25 output:
//   raf-read: java.io.IOException: Stream Closed
//   raf-read-bytes: java.io.IOException: Stream Closed
//   raf-read-zero: 0
//   raf-length: java.io.IOException: Stream Closed
//   raf-pointer: java.io.IOException: Stream Closed
//   raf-seek: java.io.IOException: Stream Closed
//   raf-seek-negative: java.io.IOException: Negative seek offset
//   raf-write: java.io.IOException: Stream Closed
//   mac-uninit-bad-range: java.lang.IllegalStateException: MAC not initialized
//   mac-bad-range: java.lang.IllegalArgumentException: Bad arguments
//   mac-null-input: ok
// Run: cratonvm --java-home <jdk25> -cp <dir> L7W102ShadowResidue   (both modes;
// writes and deletes one temp file).
public class L7W102ShadowResidue {

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String label, Row r) {
        try {
            System.out.println(label + ": " + r.run());
        } catch (Throwable t) {
            System.out.println(label + ": " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        java.io.File f = java.io.File.createTempFile("l7w102", ".bin");
        try {
            java.nio.file.Files.write(f.toPath(), new byte[] {1, 2, 3});
            java.io.RandomAccessFile raf = new java.io.RandomAccessFile(f, "rw");
            raf.close();
            row("raf-read", () -> raf.read());
            row("raf-read-bytes", () -> raf.read(new byte[4], 0, 4));
            row("raf-read-zero", () -> raf.read(new byte[4], 0, 0));
            row("raf-length", () -> raf.length());
            row("raf-pointer", () -> raf.getFilePointer());
            row("raf-seek", () -> {
                raf.seek(1);
                return "ok";
            });
            row("raf-seek-negative", () -> {
                raf.seek(-1);
                return "ok";
            });
            row("raf-write", () -> {
                raf.write(7);
                return "ok";
            });
        } finally {
            f.delete();
        }
        row("mac-uninit-bad-range", () -> {
            javax.crypto.Mac mac = javax.crypto.Mac.getInstance("HmacSHA256");
            mac.update(new byte[1], 5, 5);
            return "ok";
        });
        row("mac-bad-range", () -> {
            javax.crypto.Mac mac = javax.crypto.Mac.getInstance("HmacSHA256");
            mac.init(new javax.crypto.spec.SecretKeySpec(new byte[16], "HmacSHA256"));
            mac.update(new byte[1], 5, 5);
            return "ok";
        });
        row("mac-null-input", () -> {
            javax.crypto.Mac mac = javax.crypto.Mac.getInstance("HmacSHA256");
            mac.init(new javax.crypto.spec.SecretKeySpec(new byte[16], "HmacSHA256"));
            mac.update(null, 0, 0);
            return "ok";
        });
    }
}
