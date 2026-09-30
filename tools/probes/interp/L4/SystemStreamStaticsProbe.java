// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 6, lane L4: `getstatic System.out / err / in`
// through the bootstrap-stream intercept, interpreted and (after the loops
// warm up) compiled. Wave 6 moved both intercepts (`op_getstatic`,
// `jit_getstatic_body`) onto one screen, `system_stream_field`, which looks the
// field up among System's STATIC fields and answers without building a String,
// and made the compiled `System.in` read fall back to the canonical stdin
// object like the interpreter does instead of answering null.
//
// Expected on HotSpot 25 (and on CratonVM with and without --nojit):
//   out stable: true
//   err stable: true
//   in stable: true
//   in non-null: true
//   setIn observed: true
//   setOut observed: true
//   setErr observed: true
//   restored: true
//   other System static: true
//
// Deterministic; no timing output.

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.PrintStream;

public class SystemStreamStaticsProbe {
    static final int N = 200_000;

    static boolean outStable() {
        PrintStream first = System.out;
        boolean same = true;
        for (int i = 0; i < N; i++) {
            same &= System.out == first;
        }
        return same && first != null;
    }

    static boolean errStable() {
        PrintStream first = System.err;
        boolean same = true;
        for (int i = 0; i < N; i++) {
            same &= System.err == first;
        }
        return same && first != null;
    }

    static InputStream readIn() {
        return System.in;
    }

    static boolean inStable() {
        InputStream first = readIn();
        boolean same = true;
        for (int i = 0; i < N; i++) {
            same &= readIn() == first;
        }
        return same;
    }

    public static void main(String[] args) {
        PrintStream realOut = System.out;
        PrintStream realErr = System.err;
        InputStream realIn = System.in;

        System.out.println("out stable: " + outStable());
        System.out.println("err stable: " + errStable());
        System.out.println("in stable: " + inStable());
        System.out.println("in non-null: " + (readIn() != null));

        InputStream replacementIn = new ByteArrayInputStream(new byte[] {1, 2, 3});
        System.setIn(replacementIn);
        boolean inSeen = true;
        for (int i = 0; i < N; i++) {
            inSeen &= readIn() == replacementIn;
        }
        System.setIn(realIn);

        PrintStream replacementOut = new PrintStream(new ByteArrayOutputStream());
        System.setOut(replacementOut);
        boolean outSeen = true;
        for (int i = 0; i < N; i++) {
            outSeen &= System.out == replacementOut;
        }
        System.setOut(realOut);

        PrintStream replacementErr = new PrintStream(new ByteArrayOutputStream());
        System.setErr(replacementErr);
        boolean errSeen = true;
        for (int i = 0; i < N; i++) {
            errSeen &= System.err == replacementErr;
        }
        System.setErr(realErr);

        System.out.println("setIn observed: " + inSeen);
        System.out.println("setOut observed: " + outSeen);
        System.out.println("setErr observed: " + errSeen);
        System.out.println("restored: "
                + (System.out == realOut && System.err == realErr && System.in == realIn));
        // A System static that is not a stream still reads through the
        // ordinary arm (lineSeparator() reads a private static String).
        String sep = System.lineSeparator();
        System.out.println("other System static: " + (sep != null && !sep.isEmpty()));
    }
}
