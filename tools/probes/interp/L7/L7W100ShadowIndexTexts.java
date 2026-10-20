// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L7: the exception class AND message of
// JDK 25 APIs whose CratonVM natives stood in with "Array index out of range: n"
// AIOOBEs (or no exception at all). Every row prints `Class: message` of the
// throwable the call raises, or the value it returns. Sources read from the
// JDK 25.0.3 src.zip (`Preconditions`, `Arrays.rangeCheck`, `String`,
// `ByteArrayInputStream`, `Deflater`/`Inflater`, `InflaterInputStream`,
// `ZipOutputStream`, `DeflaterOutputStream`, `Mac`, the atomic field
// updaters); RandomAccessFile's are `io_util.c`'s.
// See docs/internal/fixed-bugs/interpreter-i100-L7-native-shadow-npe-texts-FIXED-20261016.md.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L7W100ShadowIndexTexts
// (writes and deletes one temp file). HotSpot 25 is the reference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25)
// objects-checkIndex: java.lang.IndexOutOfBoundsException: Index 5 out of bounds for length 3
// objects-checkFromToIndex: java.lang.IndexOutOfBoundsException: Range [2, 1) out of bounds for length 3
// objects-checkFromIndexSize: java.lang.IndexOutOfBoundsException: Range [2, 2 + 5) out of bounds for length 3
// objects-checkFromIndexSize-overflow: java.lang.IndexOutOfBoundsException: Range [2147483647, 2147483647 + 1) out of bounds for length 3
// bais-read-range: java.lang.IndexOutOfBoundsException: Range [2, 2 + 5) out of bounds for length 3
// bais-read-null: java.lang.NullPointerException: Cannot read the array length because "b" is null
// sort-long-from-gt-to: java.lang.IllegalArgumentException: fromIndex(3) > toIndex(1)
// sort-long-negative: java.lang.ArrayIndexOutOfBoundsException: Array index out of range: -1
// sort-long-past-end: java.lang.ArrayIndexOutOfBoundsException: Array index out of range: 5
// parallelSort-int-past-end: java.lang.ArrayIndexOutOfBoundsException: Array index out of range: 5
// string-bytes-off-len: java.lang.StringIndexOutOfBoundsException: Range [2, 2 + 5) out of bounds for length 4
// string-bytes-charset: java.lang.StringIndexOutOfBoundsException: Range [-1, -1 + 2) out of bounds for length 4
// string-hibyte: java.lang.StringIndexOutOfBoundsException: Range [1, 1 + 9) out of bounds for length 4
// string-getBytes-src: java.lang.StringIndexOutOfBoundsException: Range [1, 9) out of bounds for length 3
// string-getBytes-dst: java.lang.StringIndexOutOfBoundsException: Range [1, 1 + 3) out of bounds for length 2
// deflater-setInput: java.lang.ArrayIndexOutOfBoundsException: Range [3, 3 + 2) out of bounds for length 4
// inflater-setInput: java.lang.ArrayIndexOutOfBoundsException: Range [-1, -1 + 1) out of bounds for length 4
// inflater-inflate: java.lang.ArrayIndexOutOfBoundsException: Range [0, 0 + 9) out of bounds for length 4
// iis-read-range: java.lang.IndexOutOfBoundsException: Range [1, 1 + 4) out of bounds for length 4
// iis-read-null: java.lang.NullPointerException: null
// zos-write-range: java.lang.IndexOutOfBoundsException: Range [3, 3 + 2) out of bounds for length 4
// dos-write-range: java.lang.IndexOutOfBoundsException: null
// gzos-write-range: java.lang.IndexOutOfBoundsException: null
// raf-read-range: java.lang.IndexOutOfBoundsException: null
// raf-read-null: java.lang.NullPointerException: null
// raf-write-range: java.lang.IndexOutOfBoundsException: null
// raf-readFully-null: java.lang.NullPointerException: null
// mac-update-range: java.lang.IllegalArgumentException: Bad arguments
// updater-wrong-target: java.lang.ClassCastException: null
// updater-null-target: java.lang.ClassCastException: null
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.RandomAccessFile;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.Objects;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.zip.Deflater;
import java.util.zip.DeflaterOutputStream;
import java.util.zip.GZIPOutputStream;
import java.util.zip.Inflater;
import java.util.zip.InflaterInputStream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

public class L7W100ShadowIndexTexts {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static class Holder {
        volatile int v;
    }

    public static void main(String[] args) throws Exception {
        row("objects-checkIndex", () -> Objects.checkIndex(5, 3));
        row("objects-checkFromToIndex", () -> Objects.checkFromToIndex(2, 1, 3));
        row("objects-checkFromIndexSize", () -> Objects.checkFromIndexSize(2, 5, 3));
        row("objects-checkFromIndexSize-overflow",
            () -> Objects.checkFromIndexSize(Integer.MAX_VALUE, 1, 3));

        row("bais-read-range", () -> new ByteArrayInputStream(new byte[4]).read(new byte[3], 2, 5));
        row("bais-read-null", () -> new ByteArrayInputStream(new byte[4]).read(null, 0, 1));

        row("sort-long-from-gt-to", () -> { Arrays.sort(new long[4], 3, 1); return "sorted"; });
        row("sort-long-negative", () -> { Arrays.sort(new long[4], -1, 1); return "sorted"; });
        row("sort-long-past-end", () -> { Arrays.sort(new long[4], 0, 5); return "sorted"; });
        row("parallelSort-int-past-end", () -> { Arrays.parallelSort(new int[4], 0, 5); return "sorted"; });

        byte[] four = "abcd".getBytes(StandardCharsets.ISO_8859_1);
        row("string-bytes-off-len", () -> new String(four, 2, 5));
        row("string-bytes-charset", () -> new String(four, -1, 2, StandardCharsets.UTF_8));
        row("string-hibyte", () -> new String(four, 0, 1, 9));
        row("string-getBytes-src", () -> { "xyz".getBytes(1, 9, new byte[4], 0); return "copied"; });
        row("string-getBytes-dst", () -> { "xyzw".getBytes(1, 4, new byte[2], 1); return "copied"; });

        row("deflater-setInput", () -> { new Deflater().setInput(new byte[4], 3, 2); return "set"; });
        row("inflater-setInput", () -> { new Inflater().setInput(new byte[4], -1, 1); return "set"; });
        row("inflater-inflate", () -> new Inflater().inflate(new byte[4], 0, 9));

        ByteArrayOutputStream compressed = new ByteArrayOutputStream();
        try (DeflaterOutputStream d = new DeflaterOutputStream(compressed)) {
            d.write(four);
        }
        byte[] packed = compressed.toByteArray();
        row("iis-read-range",
            () -> new InflaterInputStream(new ByteArrayInputStream(packed)).read(new byte[4], 1, 4));
        row("iis-read-null",
            () -> new InflaterInputStream(new ByteArrayInputStream(packed)).read(null, 0, 1));

        row("zos-write-range", () -> {
            ZipOutputStream z = new ZipOutputStream(new ByteArrayOutputStream());
            z.putNextEntry(new ZipEntry("e"));
            z.write(new byte[4], 3, 2);
            return "written";
        });
        row("dos-write-range", () -> {
            new DeflaterOutputStream(new ByteArrayOutputStream()).write(new byte[4], 3, 2);
            return "written";
        });
        row("gzos-write-range", () -> {
            new GZIPOutputStream(new ByteArrayOutputStream()).write(new byte[4], 3, 2);
            return "written";
        });

        File tmp = File.createTempFile("l7w100", ".bin");
        try (RandomAccessFile raf = new RandomAccessFile(tmp, "rw")) {
            raf.write(four);
            raf.seek(0);
            row("raf-read-range", () -> raf.read(new byte[4], 3, 2));
            row("raf-read-null", () -> raf.read(null, 0, 1));
            row("raf-write-range", () -> { raf.write(new byte[4], 3, 2); return "written"; });
            row("raf-readFully-null", () -> { raf.readFully(null, 0, 1); return "read"; });
        } finally {
            tmp.delete();
        }

        row("mac-update-range", () -> {
            Mac mac = Mac.getInstance("HmacSHA256");
            mac.init(new SecretKeySpec(new byte[16], "HmacSHA256"));
            mac.update(new byte[4], 3, 2);
            return "updated";
        });

        AtomicIntegerFieldUpdater<Holder> u = AtomicIntegerFieldUpdater.newUpdater(Holder.class, "v");
        @SuppressWarnings({"unchecked", "rawtypes"})
        AtomicIntegerFieldUpdater<Object> raw = (AtomicIntegerFieldUpdater) u;
        row("updater-wrong-target", () -> raw.get(new Object()));
        row("updater-null-target", () -> u.get(null));
    }
}
