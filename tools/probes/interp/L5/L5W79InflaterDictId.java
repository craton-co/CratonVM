// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 79, lane L5 -- `Inflater.getAdler()` on a
// zlib-wrapped stream that needs a preset dictionary
// (`docs/internal/fixed-bugs/interpreter-i73-L5-inflater-getadler-differs-from-zlib-FIXED-20261111.md`,
// case 3).
//
// zlib answers Z_NEED_DICT with `strm->adler` = the DICTID read from the
// stream header (the ADLER-32 of the dictionary the stream was made with), and
// the `Inflater.needsDictionary()` javadoc tells the caller to read it with
// `getAdler()`. It survives `inflateSetDictionary`; the next `inflate` re-seeds
// it to 1 in zlib's DICT state and ends at the ADLER-32 of the output.
// CratonVM used to answer its own running output checksum (1 for a fresh
// stream) at `needsDictionary()`.
//
// Run:
//   $J/javac -d out L5W79InflaterDictId.java
//   $J/java -cp out L5W79InflaterDictId                                  # HotSpot
//   $B --java-home $J/.. -cp out L5W79InflaterDictId
//   $B --compatible --java-home $J/.. -cp out L5W79InflaterDictId
// (each CratonVM line also with --nojit).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// first n=0 needsDictionary=true adler-is-dictid=true
// after-set adler-is-dictid=true
// second equal=true finished=true adler-is-output=true
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.util.Arrays;
import java.util.zip.Adler32;
import java.util.zip.Deflater;
import java.util.zip.Inflater;

public class L5W79InflaterDictId {
    static long adler(byte[] b) {
        Adler32 a = new Adler32();
        a.update(b);
        return a.getValue();
    }

    public static void main(String[] args) throws Exception {
        byte[] dict = "optionsgetheadpostputdelete".getBytes("US-ASCII");
        byte[] payload = "options get head post put delete options get".getBytes("US-ASCII");

        Deflater d = new Deflater();
        d.setDictionary(dict);
        d.setInput(payload);
        d.finish();
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        byte[] buf = new byte[256];
        int guard = 0;
        while (!d.finished() && guard++ < 100_000) {
            bos.write(buf, 0, d.deflate(buf));
        }
        d.end();
        byte[] c = bos.toByteArray();

        Inflater inf = new Inflater();
        inf.setInput(c);
        byte[] out = new byte[256];
        int n = inf.inflate(out);
        System.out.println("first n=" + n + " needsDictionary=" + inf.needsDictionary()
                + " adler-is-dictid=" + ((inf.getAdler() & 0xFFFFFFFFL) == adler(dict)));
        inf.setDictionary(dict);
        System.out.println("after-set adler-is-dictid=" + ((inf.getAdler() & 0xFFFFFFFFL) == adler(dict)));
        int total = 0;
        guard = 0;
        while (!inf.finished() && total < out.length && guard++ < 100_000) {
            total += inf.inflate(out, total, out.length - total);
        }
        System.out.println("second equal=" + Arrays.equals(Arrays.copyOf(out, total), payload)
                + " finished=" + inf.finished()
                + " adler-is-output=" + ((inf.getAdler() & 0xFFFFFFFFL) == adler(payload)));
        inf.end();
    }
}
