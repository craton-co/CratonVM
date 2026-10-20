// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L7 (i100-L7-native-shadow-npe-texts, Lucene EOF residue):
// the native shadows of ByteBuffersDataInput's sequential reads, readBytes and seek answered
// the end of the data with their own `EOFException: Unexpected EOF`, clamped a negative
// readBytes offset to 0, and a closed ByteBuffersIndexInput with IllegalStateException. On
// HotSpot Lucene's bytecode answers (its own EOFException text for the version, the
// ByteBuffer.get / arraycopy index text, AlreadyClosedException). The shadows now re-run the
// bytecode on their own EOF / index / null refusals (`bytecode_on_own_refusal_or_eof!`).
// Expected output: HotSpot 25 with the same jar (the orchestrator records it); each row is
// `<label> -> <result or exception class: message>`.
// Setup:
//   javac -cp ~/.m2/repository/org/apache/lucene/lucene-core/9.7.0/lucene-core-9.7.0.jar -d <dir> L7W104LuceneEofRefusals.java
//   cratonvm --java-home <jdk25> -cp <dir>:<lucene-core-9.7.0.jar> L7W104LuceneEofRefusals   (both modes)
import java.nio.ByteBuffer;
import java.util.Arrays;
import java.util.List;

import org.apache.lucene.store.ByteBuffersDataInput;
import org.apache.lucene.store.ByteBuffersIndexInput;

public class L7W104LuceneEofRefusals {

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

    static ByteBuffersDataInput input() {
        return new ByteBuffersDataInput(List.of(ByteBuffer.wrap(new byte[] {1, 2, 3, 4, 5, 6, 7, 8})));
    }

    public static void main(String[] args) {
        row("eof-readByte-at-end", () -> {
            ByteBuffersDataInput in = input();
            in.seek(8);
            return "" + in.readByte();
        });
        row("eof-readInt-straddles-end", () -> {
            ByteBuffersDataInput in = input();
            in.seek(6);
            return "" + in.readInt() + " pos=" + in.position();
        });
        row("eof-readLong-at-1", () -> {
            ByteBuffersDataInput in = input();
            in.seek(1);
            return "" + in.readLong();
        });
        row("eof-seek-past-end", () -> {
            ByteBuffersDataInput in = input();
            try {
                in.seek(9);
                return "ok pos=" + in.position();
            } finally {
                System.out.println("eof-seek-past-end pos after " + in.position());
            }
        });
        row("eof-readBytes-past-end", () -> {
            ByteBuffersDataInput in = input();
            in.seek(4);
            byte[] dst = new byte[8];
            try {
                in.readBytes(dst, 0, 6);
                return "ok";
            } finally {
                System.out.println("eof-readBytes-past-end dst " + Arrays.toString(dst) + " pos " + in.position());
            }
        });
        row("eof-readBytes-negative-offset", () -> {
            ByteBuffersDataInput in = input();
            byte[] dst = new byte[4];
            in.readBytes(dst, -1, 2);
            return "ok " + Arrays.toString(dst) + " pos " + in.position();
        });
        row("eof-readBytes-dst-too-short", () -> {
            ByteBuffersDataInput in = input();
            byte[] dst = new byte[2];
            in.readBytes(dst, 1, 2);
            return "ok " + Arrays.toString(dst) + " pos " + in.position();
        });
        row("eof-readBytes-negative-len", () -> {
            ByteBuffersDataInput in = input();
            in.readBytes(new byte[2], 0, -1);
            return "ok pos " + in.position();
        });
        row("eof-index-input-closed", () -> {
            ByteBuffersIndexInput in = new ByteBuffersIndexInput(input(), "probe");
            in.close();
            return "" + in.readByte();
        });
        row("eof-index-input-readInt-at-end", () -> {
            ByteBuffersIndexInput in = new ByteBuffersIndexInput(input(), "probe");
            in.seek(7);
            return "" + in.readInt();
        });
    }
}
