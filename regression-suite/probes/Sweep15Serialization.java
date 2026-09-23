import java.io.*;
import java.util.*;

/** Sweep 15: serialization contracts — what refuses, and what it says. */
public class Sweep15Serialization {
    interface C { Object g() throws Exception; }
    static void t(String l, C c) {
        try { System.out.println("Z2 " + l + " = " + c.g()); }
        catch (Throwable x) {
            System.out.println("Z2 " + l + " = " + x.getClass().getName() + " | " + x.getMessage());
        }
    }

    static class Ok implements Serializable {
        private static final long serialVersionUID = 1L;
        int a = 7;
        String s = "x";
    }

    static class NotSer {
        int a = 1;
    }

    static class HasBadField implements Serializable {
        private static final long serialVersionUID = 1L;
        NotSer bad = new NotSer();
    }

    static class HasTransient implements Serializable {
        private static final long serialVersionUID = 1L;
        transient int skipped = 9;
        int kept = 3;
    }

    enum E implements Serializable { A, B }

    static byte[] ser(Object o) throws Exception {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        try (ObjectOutputStream os = new ObjectOutputStream(b)) {
            os.writeObject(o);
        }
        return b.toByteArray();
    }

    static Object de(byte[] b) throws Exception {
        try (ObjectInputStream is = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return is.readObject();
        }
    }

    public static void main(String[] a) {
        // ---- round trips that must WORK ---------------------------------------
        t("roundtrip_simple", () -> { Ok o = (Ok) de(ser(new Ok())); return o.a + "/" + o.s; });
        t("roundtrip_string", () -> de(ser("hello")));
        t("roundtrip_int", () -> de(ser(Integer.valueOf(42))));
        t("roundtrip_list", () -> de(ser(new ArrayList<>(List.of("a", "b")))));
        t("roundtrip_map", () -> de(ser(new HashMap<>(Map.of("k", 1)))));
        t("roundtrip_array", () -> Arrays.toString((int[]) de(ser(new int[] {1, 2}))));
        t("roundtrip_enum", () -> de(ser(E.A)));
        t("roundtrip_null", () -> String.valueOf(de(ser(null))));
        t("transient_skipped", () -> {
            HasTransient h = (HasTransient) de(ser(new HasTransient()));
            return h.skipped + "/" + h.kept;
        });
        t("identity_preserved", () -> {
            Ok o = new Ok();
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            try (ObjectOutputStream os = new ObjectOutputStream(b)) {
                os.writeObject(o);
                os.writeObject(o);
            }
            try (ObjectInputStream is =
                    new ObjectInputStream(new ByteArrayInputStream(b.toByteArray()))) {
                return is.readObject() == is.readObject();
            }
        });

        // ---- refusals -----------------------------------------------------------
        t("not_serializable", () -> ser(new NotSer()));
        t("field_not_serializable", () -> ser(new HasBadField()));
        t("read_garbage", () -> de(new byte[] {1, 2, 3, 4}));
        t("read_truncated", () -> {
            byte[] full = ser(new Ok());
            return de(Arrays.copyOf(full, full.length / 2));
        });
        t("read_empty", () -> de(new byte[0]));
        t("ois_on_empty_stream", () ->
                new ObjectInputStream(new ByteArrayInputStream(new byte[0])));
        t("readObject_after_eof", () -> {
            byte[] b = ser("x");
            try (ObjectInputStream is = new ObjectInputStream(new ByteArrayInputStream(b))) {
                is.readObject();
                return is.readObject();
            }
        });
        t("write_after_close", () -> {
            ObjectOutputStream os = new ObjectOutputStream(new ByteArrayOutputStream());
            os.close();
            os.writeObject("x");
            return "no throw";
        });

        // ---- the header and magic ------------------------------------------------
        t("magic_bytes", () -> {
            byte[] b = ser("x");
            return String.format("%02x%02x", b[0], b[1]);
        });
        t("stream_version", () -> {
            byte[] b = ser("x");
            return String.format("%02x%02x", b[2], b[3]);
        });
    }
}
