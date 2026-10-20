// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L9 -- where an implicit
// NullPointerException keeps its JEP 358 text. On HotSpot it is never the
// detailMessage: getMessage() computes it into the TRANSIENT extendedMessage,
// so a serialized-and-read-back copy answers null, and so does a cause
// message copied from the field. A constructed NPE's message is a real
// detailMessage and survives.
//
// CratonVM --compatible stores the text in detailMessage (its getMessage()
// is a Bridge reading the field), so `implicit-roundtrip` keeps the text.
// See docs/internal/fixed-bugs/interpreter-i98-L9-compatible-implicit-npe-text-is-a-detail-message-FIXED-20261015.md.
//
//   $J/javac -d out tools/probes/interp/L9/L9W98NpeMessageSerialized.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp out L9W98NpeMessageSerialized
//   $J/java -cp out L9W98NpeMessageSerialized
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25)
// implicit: Cannot read field "x" because "<parameter1>" is null
// implicit-roundtrip: null
// implicit-unread-roundtrip: null
// constructed-roundtrip: given
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;

public class L9W98NpeMessageSerialized {
    static final class P {
        int x;
    }

    static int read(P p) {
        return p.x;
    }

    static NullPointerException implicitNpe() {
        try {
            read(null);
        } catch (NullPointerException e) {
            return e;
        }
        throw new AssertionError();
    }

    static Throwable roundTrip(Throwable t) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(t);
        }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            return (Throwable) in.readObject();
        }
    }

    public static void main(String[] args) throws Exception {
        NullPointerException read = implicitNpe();
        System.out.println("implicit: " + read.getMessage());
        System.out.println("implicit-roundtrip: " + roundTrip(read).getMessage());
        System.out.println("implicit-unread-roundtrip: " + roundTrip(implicitNpe()).getMessage());
        System.out.println("constructed-roundtrip: " + roundTrip(new NullPointerException("given")).getMessage());
    }
}
