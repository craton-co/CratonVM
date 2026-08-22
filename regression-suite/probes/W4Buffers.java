import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;

/**
 * The `java.io` shadow rows no probe in this lane had reached yet
 * (`WORKER-4-2` §8): `ByteArrayOutputStream` (11 owned §1.4 rows),
 * `ByteArrayInputStream` (8), `BufferedWriter` (6), `FilterOutputStream` (5),
 * `FileOutputStream` (4), `BufferedInputStream` (1), `FilterInputStream` (2),
 * `FileDescriptor` + `FileDescriptor$1` (11), and the `java.io` exception
 * classes (`IOException` 4, `EOFException` 2, `FileNotFoundException` 2,
 * `UnsupportedEncodingException` 2).
 *
 * `ByteArrayInputStream` matters more than its row count suggests: it is the
 * one shape the `Scanner` native duck-types for, and the class `URL.openStream`
 * and `getResourceAsStream` hand back on this VM, so its state is read by
 * natives in three different crates.
 *
 * The exception classes are here because their rows are `toString`,
 * `getMessage` and the constructors — the parts a log line is made of, which no
 * assertion in the corpus looks at.
 */
public class W4Buffers {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    interface Thunk { Object call() throws Exception; }

    static void ckT(String tag, Thunk t) {
        try {
            ck(tag, t.call());
        } catch (Throwable e) {
            String m = e.getMessage();
            ck(tag, "threw:" + e.getClass().getName() + (m == null ? "" : ":" + m));
        }
    }

    static String s(byte[] b) { return new String(b, StandardCharsets.UTF_8); }

    public static void main(String[] args) throws Exception {
        byte[] payload = "0123456789".getBytes(StandardCharsets.UTF_8);

        // ---- ByteArrayInputStream ---------------------------------------
        ByteArrayInputStream bi = new ByteArrayInputStream(payload);
        ck("bais.available", bi.available());
        ck("bais.read", bi.read());
        ck("bais.available.after", bi.available());
        ck("bais.markSupported", bi.markSupported());
        bi.mark(0);
        ck("bais.readAfterMark", bi.read());
        bi.reset();
        ck("bais.readAfterReset", bi.read());
        ck("bais.skip", bi.skip(3));
        ck("bais.readAfterSkip", bi.read());
        ck("bais.skip.past", bi.skip(999));
        ck("bais.read.atEof", bi.read());
        ck("bais.available.atEof", bi.available());
        // reset() with no mark returns to the ORIGINAL offset, not to 0.
        ByteArrayInputStream off = new ByteArrayInputStream(payload, 4, 3);
        ck("bais.offset.available", off.available());
        ck("bais.offset.read", off.read());
        off.skip(99);
        off.reset();
        ck("bais.offset.afterReset", off.read());
        byte[] into = new byte[6];
        ByteArrayInputStream bi2 = new ByteArrayInputStream(payload);
        ck("bais.read3.n", bi2.read(into, 1, 4));
        ck("bais.read3.content", Arrays.toString(into));
        ck("bais.read3.zeroLen", bi2.read(into, 0, 0));
        ckT("bais.read3.oob", () -> new ByteArrayInputStream(payload).read(new byte[2], 0, 5));
        ckT("bais.read3.negOff", () -> new ByteArrayInputStream(payload).read(new byte[4], -1, 2));
        ck("bais.readAllBytes", s(new ByteArrayInputStream(payload).readAllBytes()));
        ck("bais.readNBytes", s(new ByteArrayInputStream(payload).readNBytes(4)));
        ck("bais.close.thenRead", (Object) (new Object() {
            int go() throws IOException {
                ByteArrayInputStream b = new ByteArrayInputStream(payload);
                b.close();          // contractually a NO-OP: reads still work
                return b.read();
            }
        }).go());

        // ---- ByteArrayOutputStream ---------------------------------------
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        ck("baos.size.empty", bo.size());
        bo.write('a');
        bo.write(payload, 2, 3);
        ck("baos.size", bo.size());
        ck("baos.toString", bo.toString());
        ck("baos.toString.charset", bo.toString(StandardCharsets.UTF_8));
        ck("baos.toString.name", bo.toString("UTF-8"));
        ck("baos.toByteArray", Arrays.toString(bo.toByteArray()));
        bo.reset();
        ck("baos.size.afterReset", bo.size());
        ck("baos.toString.afterReset", "[" + bo.toString() + "]");
        ByteArrayOutputStream bo2 = new ByteArrayOutputStream(2);
        bo2.write(payload, 0, 10);
        ck("baos.grows", bo2.size() + ":" + bo2.toString());
        ByteArrayOutputStream sink = new ByteArrayOutputStream();
        bo2.writeTo(sink);
        ck("baos.writeTo", sink.toString());
        ck("baos.writeBytes", (Object) (new Object() {
            String go() {
                ByteArrayOutputStream b = new ByteArrayOutputStream();
                b.writeBytes(payload);
                return b.toString();
            }
        }).go());
        // Latin-1 through the charset overload, so a decoder that ignores its
        // argument shows up as replacement characters.
        ByteArrayOutputStream latin = new ByteArrayOutputStream();
        latin.writeBytes("café".getBytes(StandardCharsets.ISO_8859_1));
        ck("baos.toString.latin1", latin.toString(StandardCharsets.ISO_8859_1));
        ck("baos.toString.asUtf8", latin.toString(StandardCharsets.UTF_8));
        ckT("baos.toString.badName", () -> new ByteArrayOutputStream().toString("no-such-charset"));

        // ---- BufferedWriter / OutputStreamWriter ---------------------------
        StringWriter sw = new StringWriter();
        try (BufferedWriter bw = new BufferedWriter(sw)) {
            bw.write("abc");
            bw.write("defgh", 1, 3);
            bw.write('!');
            bw.newLine();
            bw.write(new char[] {'x', 'y', 'z'}, 0, 2);
            bw.append("app").append('.');
            bw.flush();
            ck("bufferedwriter.beforeClose", "[" + sw.toString().replace("\n", "\\n")
                    .replace("\r", "\\r") + "]");
        }
        ck("bufferedwriter.afterClose", "[" + sw.toString().replace("\n", "\\n")
                .replace("\r", "\\r") + "]");
        ByteArrayOutputStream osw = new ByteArrayOutputStream();
        try (Writer w = new OutputStreamWriter(osw, StandardCharsets.UTF_8)) {
            w.write("café 日");
        }
        ck("outputstreamwriter.utf8", Arrays.toString(osw.toByteArray()));

        // ---- FilterOutputStream / FilterInputStream -------------------------
        ByteArrayOutputStream inner = new ByteArrayOutputStream();
        try (FilterOutputStream fo = new FilterOutputStream(inner)) {
            fo.write('A');
            fo.write("BCD".getBytes(StandardCharsets.UTF_8));
            fo.write("EFGH".getBytes(StandardCharsets.UTF_8), 1, 2);
            fo.flush();
        }
        ck("filteroutputstream", inner.toString());
        FilterInputStream fi = new FilterInputStream(new ByteArrayInputStream(payload)) {};
        ck("filterinputstream.read", fi.read());
        ck("filterinputstream.available", fi.available());
        ck("filterinputstream.skip", fi.skip(2));
        ck("filterinputstream.markSupported", fi.markSupported());

        // ---- BufferedInputStream / BufferedOutputStream ----------------------
        BufferedInputStream bis = new BufferedInputStream(new ByteArrayInputStream(payload), 4);
        ck("bis.read", bis.read());
        ck("bis.available", bis.available());
        ck("bis.markSupported", bis.markSupported());
        bis.mark(8);
        byte[] four = new byte[4];
        ck("bis.read3", bis.read(four, 0, 4) + ":" + s(four));
        bis.reset();
        ck("bis.afterReset", bis.read());
        ck("bis.skip", bis.skip(3));
        ck("bis.readAllBytes", s(bis.readAllBytes()));
        ByteArrayOutputStream boSink = new ByteArrayOutputStream();
        try (BufferedOutputStream bos = new BufferedOutputStream(boSink, 3)) {
            bos.write('a');
            ck("bos.beforeFlush", "[" + boSink.toString() + "]");
            bos.write("bcdefg".getBytes(StandardCharsets.UTF_8));
            bos.flush();
            ck("bos.afterFlush", boSink.toString());
        }

        // ---- FileOutputStream / FileDescriptor -------------------------------
        File tmp = File.createTempFile("w4buf", ".bin");
        try (FileOutputStream fo = new FileOutputStream(tmp)) {
            fo.write(payload, 0, 5);
            FileDescriptor fd = fo.getFD();
            ck("fos.getFD.valid", fd.valid());
            fd.sync();
            ck("fos.getFD.syncOk", "ok");
        }
        ck("fos.wrote", s(java.nio.file.Files.readAllBytes(tmp.toPath())));
        try (FileInputStream fin = new FileInputStream(tmp)) {
            ck("fis.getFD.valid", fin.getFD().valid());
            ck("fis.available", fin.available());
            ck("fis.readAllBytes", s(fin.readAllBytes()));
        }
        ck("FileDescriptor.in.valid", FileDescriptor.in.valid());
        ck("FileDescriptor.out.valid", FileDescriptor.out.valid());
        ck("FileDescriptor.err.valid", FileDescriptor.err.valid());
        ck("FileDescriptor.new.valid", new FileDescriptor().valid());
        tmp.delete();

        // ---- the java.io exception classes -----------------------------------
        ck("ioe.msg", new IOException("boom").getMessage());
        ck("ioe.toString", new IOException("boom").toString());
        ck("ioe.noMsg.toString", new IOException().toString());
        ck("ioe.noMsg.getMessage", new IOException().getMessage());
        ck("ioe.cause", new IOException(new IllegalStateException("root")).getMessage());
        ck("ioe.causeToString", new IOException(new IllegalStateException("root")).toString());
        ck("ioe.msgAndCause.cause",
                new IOException("m", new IllegalStateException("root")).getCause().getMessage());
        ck("eof.toString", new EOFException("e").toString());
        ck("eof.noMsg.toString", new EOFException().toString());
        ck("eof.isIOException", new EOFException() instanceof IOException);
        ck("fnf.toString", new FileNotFoundException("/no/such").toString());
        ck("fnf.msg", new FileNotFoundException("/no/such").getMessage());
        ck("uee.toString", new UnsupportedEncodingException("cp-nope").toString());
        ck("uee.isIOException", new UnsupportedEncodingException() instanceof IOException);
        ck("utfdfe.toString", new UTFDataFormatException("bad").toString());
        ckT("fnf.thrownByFileInputStream", () -> new FileInputStream("/definitely/not/here"));
        ckT("uee.thrownByString", () -> "x".getBytes("no-such-charset"));

        System.out.println("PASS W4Buffers");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
