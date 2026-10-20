// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L8 -- record deserialization against a
// stream whose fields do not match the local record exactly. CratonVM serves
// `ObjectStreamClass$RecordSupport.deserializationCtr` natively
// (`lang_invoke.rs`, `native_record_support_deserialization_ctr` /
// `record_deser_dispatch`); the JDK builds a handle whose per-component
// extractor ends in `asType(changeReturnType(componentType))`.
//
//   swap-cast   the descriptor's two field NAMES swapped (patched stream), so
//               the String component's stream value is an Integer: the
//               extractor's cast fails, `readRecord` wraps it as
//               InvalidObjectException(cce.getMessage(), cce) (CratonVM built
//               the record with an Integer in its String component -- FIXED
//               wave 98)
//   rename      the stream field `y` renamed `z`: `y` takes its default, `z`
//               is ignored
//   retype      the stream field `y` typed float: matchFields refuses
//   plain       the unpatched stream
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L8/L8W98RecordStreamShapes.java
//   $B --java-home $J/.. -cp out L8W98RecordStreamShapes
//   $B --compatible --java-home $J/.. -cp out L8W98RecordStreamShapes
//   (each also with --nojit)
//   $J/java -cp out L8W98RecordStreamShapes
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 sources, not measured)
// plain Pair[a=5, b=y] Pt[x=5, y=6]
// swap-cast java.io.InvalidObjectException: Cannot cast java.lang.Integer to java.lang.String cause=java.lang.ClassCastException
// rename Pt[x=5, y=0]
// retype java.io.InvalidClassException: L8W98RecordStreamShapes$Pt; incompatible types for field y
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;

public class L8W98RecordStreamShapes {
    record Pair(Object a, String b) implements Serializable {
    }

    record Pt(int x, int y) implements Serializable {
    }

    static byte[] write(Object o) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(o);
        }
        return bytes.toByteArray();
    }

    static Object read(byte[] b) throws IOException, ClassNotFoundException {
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return in.readObject();
        }
    }

    /** Index of the field-descriptor head (type code, u2 length 1, name). */
    static int fieldAt(byte[] s, char type, char name) {
        for (int i = 0; i + 4 <= s.length; i++) {
            if (s[i] == type && s[i + 1] == 0 && s[i + 2] == 1 && s[i + 3] == name) {
                return i;
            }
        }
        throw new AssertionError("no field " + type + name);
    }

    static String outcome(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    public static void main(String[] args) throws Exception {
        byte[] pair = write(new Pair(5, "y"));
        byte[] pt = write(new Pt(5, 6));
        System.out.println("plain " + read(pair) + " " + read(pt));

        byte[] swapped = pair.clone();
        int a = fieldAt(swapped, 'L', 'a');
        int b = fieldAt(swapped, 'L', 'b');
        swapped[a + 3] = 'b';
        swapped[b + 3] = 'a';
        try {
            System.out.println("swap-cast no-throw " + read(swapped));
        } catch (Exception e) {
            System.out.println("swap-cast " + outcome(e)
                    + (e.getCause() == null ? "" : " cause=" + e.getCause().getClass().getName()));
        }

        byte[] renamed = pt.clone();
        renamed[fieldAt(renamed, 'I', 'y') + 3] = 'z';
        try {
            System.out.println("rename " + read(renamed));
        } catch (Exception e) {
            System.out.println("rename " + outcome(e));
        }

        byte[] retyped = pt.clone();
        retyped[fieldAt(retyped, 'I', 'y')] = 'F';
        try {
            System.out.println("retype no-throw " + read(retyped));
        } catch (Exception e) {
            System.out.println("retype " + outcome(e));
        }
    }
}
