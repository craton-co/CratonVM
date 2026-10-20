// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 74, lane L5 -- what `Inflater.getAdler()` answers
// for a raw (`nowrap`) stream, the kind every `ZipFile` entry read uses
// (`docs/internal/fixed-bugs/interpreter-i73-L5-inflater-getadler-differs-from-zlib-FIXED-20261111.md`,
// case 1). zlib keeps `strm->adler` only for a wrapped stream and the JDK
// `calloc`s the `z_stream`, so HotSpot's raw inflater should answer 0 fresh,
// after inflating and after `reset()`; a wrapped one answers zlib's seed 1
// fresh and after `reset()`, and the ADLER-32 of the output after a stream.
//
// CratonVM answers the expected block with CRATONVM_INFLATER_RAW_ADLER_ZERO=1;
// without it the four `raw` lines carrying `adler=` show 1 / the output's checksum /
// 1 / the output's checksum (the defect). Every other line is the same either way. The compressed sizes
// are not printed (the two VMs' deflaters may choose different encodings); the
// inflater's totals are printed only where they are the plain length or compared
// with the compressed length.
//
// Run:
//   $J/javac -d out L5W74InflaterAdler.java
//   $J/java -cp out L5W74InflaterAdler                                              # HotSpot
//   env CRATONVM_INFLATER_RAW_ADLER_ZERO=1 $B --java-home $J/.. -cp out L5W74InflaterAdler
//   $B --java-home $J/.. -cp out L5W74InflaterAdler                                 # default: the raw lines differ
// (each CratonVM line also with `--compatible`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// raw fresh adler=0
// raw inflated=10000 equal=true finished=true
// raw after adler=0
// raw totals in-is-compressed=true out=10000 read-is-compressed=true written=10000 remaining=0
// raw after-reset adler=0 out=0 written=0
// raw second inflated=10000 equal=true adler=0
// wrapped fresh adler=1
// wrapped inflated=10000 equal=true finished=true
// wrapped after adler-is-output-adler32=true
// wrapped totals in-is-compressed=true out=10000 read-is-compressed=true written=10000 remaining=0
// wrapped after-reset adler=1 out=0 written=0
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.util.Arrays;
import java.util.zip.Adler32;
import java.util.zip.DataFormatException;
import java.util.zip.Deflater;
import java.util.zip.Inflater;

public class L5W74InflaterAdler {
    static byte[] plain() {
        byte[] b = new byte[10000];
        for (int i = 0; i < b.length; i++) {
            b[i] = (byte) ((i * 13 + (i >> 7)) % 241);
        }
        return b;
    }

    static byte[] deflate(byte[] input, boolean nowrap) {
        Deflater deflater = new Deflater(Deflater.DEFAULT_COMPRESSION, nowrap);
        try {
            deflater.setInput(input);
            deflater.finish();
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            while (!deflater.finished()) {
                out.write(buf, 0, deflater.deflate(buf));
            }
            return out.toByteArray();
        } finally {
            deflater.end();
        }
    }

    /** Inflates `compressed` in 1000-byte output windows; answers the bytes produced. */
    static byte[] inflate(Inflater inflater, byte[] compressed, int size) throws DataFormatException {
        inflater.setInput(compressed);
        byte[] out = new byte[size];
        int got = 0;
        while (got < size && !inflater.finished()) {
            int k = inflater.inflate(out, got, Math.min(1000, size - got));
            if (k == 0 && (inflater.needsInput() || inflater.needsDictionary())) {
                break;
            }
            got += k;
        }
        return Arrays.copyOf(out, got);
    }

    static void run(String label, boolean nowrap) throws DataFormatException {
        byte[] plain = plain();
        byte[] compressed = deflate(plain, nowrap);
        Adler32 check = new Adler32();
        check.update(plain);
        Inflater inflater = new Inflater(nowrap);
        try {
            System.out.println(label + " fresh adler=" + inflater.getAdler());
            byte[] out = inflate(inflater, compressed, plain.length + 16);
            System.out.println(label + " inflated=" + out.length + " equal=" + Arrays.equals(out, plain)
                    + " finished=" + inflater.finished());
            if (nowrap) {
                System.out.println(label + " after adler=" + inflater.getAdler());
            } else {
                System.out.println(label + " after adler-is-output-adler32="
                        + (inflater.getAdler() == (int) check.getValue()));
            }
            System.out.println(label + " totals in-is-compressed=" + (inflater.getTotalIn() == compressed.length)
                    + " out=" + inflater.getTotalOut()
                    + " read-is-compressed=" + (inflater.getBytesRead() == compressed.length)
                    + " written=" + inflater.getBytesWritten()
                    + " remaining=" + inflater.getRemaining());
            inflater.reset();
            System.out.println(label + " after-reset adler=" + inflater.getAdler()
                    + " out=" + inflater.getTotalOut() + " written=" + inflater.getBytesWritten());
            if (nowrap) {
                byte[] again = inflate(inflater, compressed, plain.length + 16);
                System.out.println(label + " second inflated=" + again.length + " equal="
                        + Arrays.equals(again, plain) + " adler=" + inflater.getAdler());
            }
        } finally {
            inflater.end();
        }
    }

    public static void main(String[] args) throws Exception {
        run("raw", true);
        run("wrapped", false);
    }
}
