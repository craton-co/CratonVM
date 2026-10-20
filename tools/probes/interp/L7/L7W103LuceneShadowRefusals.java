// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L7 (i100-L7-native-shadow-npe-texts, Lucene residue):
// the native shadows of ByteBuffersDataInput's absolute reads and readFloats / readLongs
// refused an out-of-range position or a bad destination range with "Array index out of
// range: n". On HotSpot Lucene's bytecode answers (an IndexOutOfBoundsException from
// ByteBuffer.get, Lucene's own EOFException, the JVM's array-store text ...), and its text
// depends on the Lucene version. The shadows now re-run Lucene's bytecode on such a refusal.
// Expected output: HotSpot 25 with the same jar (the orchestrator records it).
// Setup:
//   javac -cp ~/.m2/repository/org/apache/lucene/lucene-core/9.7.0/lucene-core-9.7.0.jar -d <dir> L7W103LuceneShadowRefusals.java
//   cratonvm --java-home <jdk25> -cp <dir>:<lucene-core-9.7.0.jar> L7W103LuceneShadowRefusals   (both modes)
import java.nio.ByteBuffer;
import java.util.List;

import org.apache.lucene.store.ByteBuffersDataInput;

public class L7W103LuceneShadowRefusals {

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

    public static void main(String[] args) {
        ByteBuffer bb = ByteBuffer.wrap(new byte[] {1, 2, 3, 4, 5, 6, 7, 8});
        ByteBuffersDataInput in = new ByteBuffersDataInput(List.of(bb));
        row("readByte-at-end", () -> "" + in.readByte(8L));
        row("readByte-negative", () -> "" + in.readByte(-1L));
        row("readInt-straddles-end", () -> "" + in.readInt(6L));
        row("readLong-at-0", () -> "" + in.readLong(0L));
        row("readFloats-bad-offset", () -> {
            in.seek(0);
            in.readFloats(new float[2], 1, 2);
            return "ok pos=" + in.position();
        });
        row("readLongs-negative-offset", () -> {
            in.seek(0);
            in.readLongs(new long[2], -1, 1);
            return "ok pos=" + in.position();
        });
        row("readFloats-ok", () -> {
            in.seek(0);
            float[] f = new float[2];
            in.readFloats(f, 0, 2);
            return f.length + " pos=" + in.position();
        });
    }
}
