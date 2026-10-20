// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 79, lane L5 -- `Deflater` parameter changes and
// `Deflater.getAdler()`:
//   docs/internal/fixed-bugs/interpreter-i75-L5-deflater-params-ignore-the-strategy-and-deflate-in-the-same-call-FIXED-20261111.md
//   docs/internal/fixed-bugs/interpreter-i73-L5-inflater-getadler-differs-from-zlib-FIXED-20261111.md (case 2)
//
// HotSpot's `Deflater.c` runs ONLY `deflateParams(strm, level, strategy)` in a
// call that carries `setParams`, and reports bit 63 ("still pending") when it
// answers `Z_BUF_ERROR`, so `Deflater.deflate` keeps the change and retries.
// CratonVM used to drop the strategy (`flate2::Compress::set_level`), ignore
// the `Z_BUF_ERROR` (the change was lost) and compress in the same call. Since
// wave 79 the deflater is zlib's own `z_stream`, driven as `Deflater.c` does.
//
// No compressed byte is printed (it depends on the zlib build); each line is a
// coarse fact that only the strategy / level actually taking effect decides:
//   * `huffman`: 10 000 'a' bytes with HUFFMAN_ONLY cannot use LZ77 matches
//     (about a bit per byte, > 1000 bytes); the default strategy needs < 100.
//   * `level`: a 4 KiB block left pending by a SYNC_FLUSH into a 16-byte buffer
//     makes the next `deflateParams` answer Z_BUF_ERROR; the change to
//     NO_COMPRESSION must survive it, so the 8 KiB of 'a' fed afterwards is
//     stored (> 8192 bytes), not compressed.
//   * `adler`: zlib's `strm->adler` -- 1 forever for a raw stream, the
//     dictionary's ADLER-32 right after `setDictionary`.
//
// Run:
//   $J/javac -d out L5W79DeflaterParams.java
//   $J/java -cp out L5W79DeflaterParams                                  # HotSpot
//   $B --java-home $J/.. -cp out L5W79DeflaterParams
//   $B --compatible --java-home $J/.. -cp out L5W79DeflaterParams
// (each CratonVM line also with --nojit).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// huffman over1000=true roundtrip=true
// default under100=true roundtrip=true
// level first=16 needsInput=true
// level tail-stored=true roundtrip=true
// adler raw-fresh=1 raw-after=1
// adler wrapped-matches=true
// adler dict-matches=true
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.util.Arrays;
import java.util.zip.Adler32;
import java.util.zip.Deflater;
import java.util.zip.Inflater;

public class L5W79DeflaterParams {
    static final int GUARD = 1_000_000;

    static byte[] deflateToEnd(Deflater d, byte[] in, ByteArrayOutputStream bos) {
        d.setInput(in);
        d.finish();
        byte[] buf = new byte[16384];
        int guard = 0;
        while (!d.finished() && guard++ < GUARD) {
            int n = d.deflate(buf);
            bos.write(buf, 0, n);
        }
        return bos.toByteArray();
    }

    static byte[] inflateRaw(byte[] c, int len) throws Exception {
        Inflater inf = new Inflater(true);
        inf.setInput(Arrays.copyOf(c, c.length + 1));
        byte[] out = new byte[len];
        int n = 0;
        int guard = 0;
        while (!inf.finished() && n < len && guard++ < GUARD) {
            n += inf.inflate(out, n, len - n);
        }
        inf.end();
        return Arrays.copyOf(out, n);
    }

    static long adler(byte[] b) {
        Adler32 a = new Adler32();
        a.update(b);
        return a.getValue();
    }

    public static void main(String[] args) throws Exception {
        byte[] as = new byte[10_000];
        Arrays.fill(as, (byte) 'a');

        Deflater huff = new Deflater(Deflater.DEFAULT_COMPRESSION, true);
        huff.setStrategy(Deflater.HUFFMAN_ONLY);
        byte[] h = deflateToEnd(huff, as, new ByteArrayOutputStream());
        huff.end();
        System.out.println("huffman over1000=" + (h.length > 1000)
                + " roundtrip=" + Arrays.equals(inflateRaw(h, as.length), as));

        Deflater plain = new Deflater(Deflater.DEFAULT_COMPRESSION, true);
        byte[] p = deflateToEnd(plain, as, new ByteArrayOutputStream());
        plain.end();
        System.out.println("default under100=" + (p.length < 100)
                + " roundtrip=" + Arrays.equals(inflateRaw(p, as.length), as));

        // A level change that meets Z_BUF_ERROR.
        byte[] rnd = new byte[4096];
        int seed = 0x12345678;
        for (int i = 0; i < rnd.length; i++) {
            seed = seed * 1103515245 + 12345;
            rnd[i] = (byte) (seed >>> 24);
        }
        byte[] tailIn = new byte[8192];
        Arrays.fill(tailIn, (byte) 'a');
        Deflater d = new Deflater(Deflater.DEFAULT_COMPRESSION, true);
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        byte[] small = new byte[16];
        d.setInput(rnd);
        int n = d.deflate(small, 0, small.length, Deflater.SYNC_FLUSH);
        bos.write(small, 0, n);
        System.out.println("level first=" + n + " needsInput=" + d.needsInput());
        d.setLevel(Deflater.NO_COMPRESSION);
        int guard = 0;
        while ((n = d.deflate(small, 0, small.length, Deflater.NO_FLUSH)) > 0 && guard++ < GUARD) {
            bos.write(small, 0, n);
        }
        int head = bos.size();
        byte[] all = deflateToEnd(d, tailIn, bos);
        d.end();
        byte[] want = new byte[rnd.length + tailIn.length];
        System.arraycopy(rnd, 0, want, 0, rnd.length);
        System.arraycopy(tailIn, 0, want, rnd.length, tailIn.length);
        System.out.println("level tail-stored=" + (all.length - head > 8192)
                + " roundtrip=" + Arrays.equals(inflateRaw(all, want.length), want));

        // getAdler() is zlib's strm->adler.
        byte[] text = "hello hello hello world".getBytes("US-ASCII");
        Deflater raw = new Deflater(Deflater.DEFAULT_COMPRESSION, true);
        int rawFresh = raw.getAdler();
        deflateToEnd(raw, text, new ByteArrayOutputStream());
        int rawAfter = raw.getAdler();
        raw.end();
        System.out.println("adler raw-fresh=" + rawFresh + " raw-after=" + rawAfter);

        Deflater wrapped = new Deflater();
        deflateToEnd(wrapped, text, new ByteArrayOutputStream());
        System.out.println("adler wrapped-matches=" + ((wrapped.getAdler() & 0xFFFFFFFFL) == adler(text)));
        wrapped.end();

        byte[] dict = "optionsgetheadpostputdelete".getBytes("US-ASCII");
        Deflater withDict = new Deflater();
        withDict.setDictionary(dict);
        System.out.println("adler dict-matches=" + ((withDict.getAdler() & 0xFFFFFFFFL) == adler(dict)));
        withDict.end();
    }
}
