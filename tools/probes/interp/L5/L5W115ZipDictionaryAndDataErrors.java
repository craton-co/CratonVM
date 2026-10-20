// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L5: what `java.util.zip.Inflater` /
// `Deflater` throw for a corrupt stream and for a dictionary zlib refuses
// (page `docs/internal/fixed-bugs/interpreter-i115-L5-zip-data-error-and-dictionary-refusal-texts-FIXED-20261019.md`).
//
// HotSpot's `Inflater.c` throws `DataFormatException(strm->msg)`: zlib's own
// text ("incorrect header check"), where CratonVM threw flate2's Display
// ("deflate decompression error: incorrect header check"). Its
// `checkSetDictionaryResult` (both classes) throws `IllegalArgumentException`
// when zlib refuses the dictionary (a zlib stream not at its DICT point, a
// dictionary whose Adler-32 is not the stream's DICTID, a deflater past its
// header), where CratonVM swallowed the refusal.
//
// Run: javac -d . L5W115ZipDictionaryAndDataErrors.java
//      java -cp . L5W115ZipDictionaryAndDataErrors                 (HotSpot reference)
//      cratonvm --jdk-only -cp . L5W115ZipDictionaryAndDataErrors
//      cratonvm --compatible -cp . L5W115ZipDictionaryAndDataErrors
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// not-zlib java.util.zip.DataFormatException: incorrect header check
// bad-block-type java.util.zip.DataFormatException: invalid block type
// bad-adler java.util.zip.DataFormatException: incorrect data check
// bad-distance java.util.zip.DataFormatException: invalid distance too far back
// raw-bad-block java.util.zip.DataFormatException: invalid block type
// after-error-remaining 6
// fresh-dict java.lang.IllegalArgumentException: null
// needs-dict true adler 823592489
// wrong-dict java.lang.IllegalArgumentException: null
// after-wrong-needs true
// after-wrong-inflate 0 needs true
// right-dict ok
// right-dict-text dictionary-text dictionary-text
// raw-dict-anytime ok
// defl-fresh-dict ok
// defl-late-dict java.lang.IllegalArgumentException: null
// defl-raw-late-dict ok
// defl-buffer-late-dict java.lang.IllegalArgumentException: null
// infl-buffer-fresh-dict java.lang.IllegalArgumentException: null
// HOTSPOT_EXPECTED_END

import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.zip.DataFormatException;
import java.util.zip.Deflater;
import java.util.zip.Inflater;

public class L5W115ZipDictionaryAndDataErrors {
    static String inflateAll(Inflater inf, byte[] in) {
        inf.setInput(in);
        byte[] out = new byte[256];
        try {
            int n = 0;
            while (!inf.finished() && !inf.needsInput() && !inf.needsDictionary()) {
                int k = inf.inflate(out, n, out.length - n);
                n += k;
                if (k == 0 && n == out.length) {
                    break;
                }
            }
            return "ok " + n;
        } catch (DataFormatException e) {
            return e.toString();
        }
    }

    static String call(Runnable r) {
        try {
            r.run();
            return "ok";
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static byte[] zlib(byte[] data, byte[] dict, boolean nowrap) {
        Deflater d = new Deflater(Deflater.DEFAULT_COMPRESSION, nowrap);
        if (dict != null) {
            d.setDictionary(dict);
        }
        d.setInput(data);
        d.finish();
        byte[] buf = new byte[512];
        int n = d.deflate(buf);
        d.end();
        byte[] r = new byte[n];
        System.arraycopy(buf, 0, r, 0, n);
        return r;
    }

    public static void main(String[] args) throws Exception {
        // 1. Not a zlib stream (CM=1, header check fails).
        System.out.println("not-zlib " + inflateAll(new Inflater(), new byte[] {1, 2, 3, 4, 5, 6, 7, 8}));
        // 2. A valid zlib header, then a block of type 3 (reserved): BFINAL=1, BTYPE=11.
        System.out.println("bad-block-type " + inflateAll(new Inflater(), new byte[] {0x78, (byte) 0x9c, 0x07, 0, 0, 0}));
        // 3. A good stream with its trailing Adler-32 flipped.
        byte[] good = zlib("hello hello hello hello".getBytes(StandardCharsets.US_ASCII), null, false);
        byte[] badAdler = good.clone();
        badAdler[badAdler.length - 1] ^= 0x55;
        System.out.println("bad-adler " + inflateAll(new Inflater(), badAdler));
        // 4. A fixed-Huffman block whose first symbol is a match at distance 1 with no
        //    output yet: "invalid distance too far back".
        //    BFINAL=1 BTYPE=01, then length code 257 (len 3, 7 bits 0000001), distance code 0.
        System.out.println("bad-distance " + inflateAll(new Inflater(), new byte[] {0x78, (byte) 0x9c, 0x03, 0x02, 0, 0, 0, 0}));
        // 5. Raw (nowrap) stream with a reserved block type.
        System.out.println("raw-bad-block " + inflateAll(new Inflater(true), new byte[] {0x07, 0, 0, 0}));
        // 6. Progress fields survive the throw: the header is consumed, the rest remains.
        Inflater partial = new Inflater();
        partial.setInput(new byte[] {0x78, (byte) 0x9c, 0x07, 0, 0, 0, 0, 0, 0});
        try {
            partial.inflate(new byte[16]);
        } catch (DataFormatException e) {
            // expected
        }
        System.out.println("after-error-remaining " + partial.getRemaining());

        // 7. A dictionary on a fresh zlib inflater (no input yet): zlib's Z_STREAM_ERROR.
        System.out.println("fresh-dict " + call(() -> new Inflater().setDictionary(new byte[] {1, 2, 3})));

        // 8. A stream that needs a dictionary: a wrong one, then the right one.
        byte[] dict = "dictionary-text".getBytes(StandardCharsets.US_ASCII);
        byte[] data = "dictionary-text dictionary-text".getBytes(StandardCharsets.US_ASCII);
        byte[] withDict = zlib(data, dict, false);
        Inflater needs = new Inflater();
        needs.setInput(withDict);
        byte[] out = new byte[256];
        int n0 = needs.inflate(out);
        System.out.println("needs-dict " + needs.needsDictionary() + " adler " + needs.getAdler()
                + (n0 == 0 ? "" : " produced " + n0));
        System.out.println("wrong-dict " + call(() -> needs.setDictionary("other".getBytes(StandardCharsets.US_ASCII))));
        System.out.println("after-wrong-needs " + needs.needsDictionary());
        int n1 = needs.inflate(out);
        System.out.println("after-wrong-inflate " + n1 + " needs " + needs.needsDictionary());
        System.out.println("right-dict " + call(() -> needs.setDictionary(dict)));
        int n2 = needs.inflate(out);
        System.out.println("right-dict-text " + new String(out, 0, n2, StandardCharsets.US_ASCII));
        needs.end();

        // 9. A raw inflater takes a dictionary at any time.
        System.out.println("raw-dict-anytime " + call(() -> new Inflater(true).setDictionary(dict)));

        // 10. Deflater: a dictionary before the first deflate is fine; after the
        //     header is out zlib refuses it (wrap 1, status no longer INIT).
        System.out.println("defl-fresh-dict " + call(() -> new Deflater().setDictionary(dict)));
        Deflater late = new Deflater();
        late.setInput(data);
        late.deflate(new byte[256], 0, 256, Deflater.SYNC_FLUSH);
        System.out.println("defl-late-dict " + call(() -> late.setDictionary(dict)));
        late.end();
        // A raw deflater after a SYNC_FLUSH (nothing buffered) accepts it.
        Deflater rawLate = new Deflater(Deflater.DEFAULT_COMPRESSION, true);
        rawLate.setInput(data);
        rawLate.deflate(new byte[256], 0, 256, Deflater.SYNC_FLUSH);
        System.out.println("defl-raw-late-dict " + call(() -> rawLate.setDictionary(dict)));
        rawLate.end();

        // 11. The direct-buffer overloads take the same path.
        ByteBuffer direct = ByteBuffer.allocateDirect(dict.length);
        direct.put(dict).flip();
        Deflater lateBuf = new Deflater();
        lateBuf.setInput(data);
        lateBuf.deflate(new byte[256], 0, 256, Deflater.SYNC_FLUSH);
        System.out.println("defl-buffer-late-dict " + call(() -> lateBuf.setDictionary(direct.duplicate())));
        lateBuf.end();
        System.out.println("infl-buffer-fresh-dict " + call(() -> new Inflater().setDictionary(direct.duplicate())));
    }
}
