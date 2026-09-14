import java.io.*;
import java.math.*;
import java.util.*;

/** Sweep 13: what a failed PARSE says, and what a CLOSED stream does. */
public class Sweep13ParseAndStreams {
    interface Call { Object get() throws Exception; }
    static void t(String l, Call c) {
        try { System.out.println("N " + l + " = " + c.get()); }
        catch (Throwable x) {
            System.out.println("N " + l + " = " + x.getClass().getName() + " | " + x.getMessage());
        }
    }

    public static void main(String[] a) throws Exception {
        // ---- Integer / Long parsing ------------------------------------------
        t("int_empty", () -> Integer.parseInt(""));
        t("int_null", () -> Integer.parseInt(null));
        t("int_space", () -> Integer.parseInt(" 1"));
        t("int_plus", () -> Integer.parseInt("+1"));
        t("int_alpha", () -> Integer.parseInt("12x"));
        t("int_overflow", () -> Integer.parseInt("2147483648"));
        t("int_underflow", () -> Integer.parseInt("-2147483649"));
        t("int_sign_only", () -> Integer.parseInt("-"));
        t("int_radix_2", () -> Integer.parseInt("101", 2));
        t("int_radix_bad_digit", () -> Integer.parseInt("2", 2));
        t("int_radix_37", () -> Integer.parseInt("1", 37));
        t("int_radix_1", () -> Integer.parseInt("1", 1));
        t("int_valueOf_empty", () -> Integer.valueOf(""));
        t("int_decode_bad", () -> Integer.decode("0xZZ"));
        t("long_overflow", () -> Long.parseLong("9223372036854775808"));
        t("long_alpha", () -> Long.parseLong("1a"));
        t("short_overflow", () -> Short.parseShort("32768"));
        t("byte_overflow", () -> Byte.parseByte("128"));
        t("uint_parse", () -> Integer.parseUnsignedInt("-1"));

        // ---- floating point ---------------------------------------------------
        t("double_empty", () -> Double.parseDouble(""));
        t("double_null", () -> Double.parseDouble(null));
        t("double_alpha", () -> Double.parseDouble("1.0x"));
        t("double_nan_ok", () -> Double.parseDouble("NaN"));
        t("double_inf_ok", () -> Double.parseDouble("Infinity"));
        t("double_hex_ok", () -> Double.parseDouble("0x1p3"));
        t("float_alpha", () -> Float.parseFloat("q"));

        // ---- BigInteger / BigDecimal -------------------------------------------
        t("bigint_empty", () -> new BigInteger(""));
        t("bigint_alpha", () -> new BigInteger("12x"));
        t("bigint_radix", () -> new BigInteger("ff", 16));
        t("bigdec_empty", () -> new BigDecimal(""));
        t("bigdec_alpha", () -> new BigDecimal("1.0.0"));
        t("bigdec_divide_zero", () -> BigDecimal.ONE.divide(BigDecimal.ZERO));
        t("bigdec_nonterminating", () -> BigDecimal.ONE.divide(new BigDecimal("3")));

        // ---- streams after close -----------------------------------------------
        t("bais_read_after_close", () -> {
            ByteArrayInputStream s = new ByteArrayInputStream(new byte[] {1, 2});
            s.close();
            return s.read();
        });
        t("baos_write_after_close", () -> {
            ByteArrayOutputStream s = new ByteArrayOutputStream();
            s.close();
            s.write(7);
            return s.size();
        });
        t("sreader_read_after_close", () -> {
            StringReader r = new StringReader("ab");
            r.close();
            return r.read();
        });
        t("swriter_write_after_close", () -> {
            StringWriter w = new StringWriter();
            w.close();
            w.write('x');
            return w.toString();
        });
        t("dis_readInt_eof", () -> new DataInputStream(
                new ByteArrayInputStream(new byte[] {1})).readInt());
        t("dis_readFully_eof", () -> {
            byte[] b = new byte[4];
            new DataInputStream(new ByteArrayInputStream(new byte[] {1})).readFully(b);
            return "no throw";
        });
        t("pushback_unread_full", () -> {
            PushbackInputStream p = new PushbackInputStream(
                    new ByteArrayInputStream(new byte[] {1}));
            p.unread(1);
            p.unread(2);
            return "no throw";
        });

        // ---- mark / reset -------------------------------------------------------
        t("bais_reset_no_mark_ok", () -> {
            ByteArrayInputStream s = new ByteArrayInputStream(new byte[] {1, 2});
            s.read();
            s.reset();
            return s.read();
        });
        t("sreader_reset_no_mark", () -> {
            StringReader r = new StringReader("ab");
            r.read();
            r.reset();
            return r.read();
        });
        t("bufreader_reset_no_mark", () -> {
            BufferedReader r = new BufferedReader(new StringReader("ab"));
            r.read();
            r.reset();
            return "no throw";
        });
        t("bufreader_mark_negative", () -> {
            BufferedReader r = new BufferedReader(new StringReader("ab"));
            r.mark(-1);
            return "no throw";
        });

        // ---- negative / bad arguments -------------------------------------------
        t("bais_read_bad_off", () -> new ByteArrayInputStream(new byte[] {1})
                .read(new byte[2], 5, 1));
        t("bais_ctor_bad_range", () -> new ByteArrayInputStream(new byte[] {1}, 5, 1).read());
        t("baos_ctor_negative", () -> new ByteArrayOutputStream(-1));
        t("sreader_skip_negative", () -> new StringReader("ab").skip(-1));
    }
}
