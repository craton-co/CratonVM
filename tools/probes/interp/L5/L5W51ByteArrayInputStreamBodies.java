// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 51, lane L5 -- the I/O rows of
// `docs/internal/fixed-bugs/interpreter-L5-bridges-still-run-over-jdk-bytecode-under-jdk-only-FIXED-20261007.md`:
// `ByteArrayInputStream`'s `read()`, `read(byte[],int,int)`, `available()`,
// `skip(long)` and `close()` were natives registered over the JDK's concrete
// bodies (a `Bridge` in `native-builtins`, `SyntheticStub`s in `native-io`
// and `classloader.rs`). With `CRATONVM_BAIS_JDK_BODIES` the registry drops
// every copy over a real JDK, in both modes, and the JDK's bodies run.
//
// Rows (the first runs on a cold VM, and `own` again last): a stream the
// program builds (`own`), one the VM mints for a resource (`resource`, read
// through `DataInputStream`), the out-of-range `read(b, off, len)` and its
// message (`range`), `skip` of a negative count, `mark` / `reset`, and a
// read after `close`. The wave-50 head already printed HotSpot's lines in
// both modes (the natives agreed with the bodies on every row here), so this
// probe is the regression guard of the retirement, not its evidence: the
// retirement is AGENTS.md's classification rule (real bytes win over a
// `Bridge`), and its positive control is the report below.
//
// Run (no setup):
//   javac -d out L5W51ByteArrayInputStreamBodies.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W51ByteArrayInputStreamBodies
//
// Positive control: `--jdk-only-report r.json` names no
// `java/io/ByteArrayInputStream` row as `native-won` with the switch on
// (`read([BII)I` and `close()V` are `bridge-ran-over-bytecode`, `native-won`
// without it).
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   own-1 first=104 avail=4 read3=3 [101, 108, 108] skip=1 then=-1 avail=0
//   resource magic=cafebabe more=true
//   range=java.lang.IndexOutOfBoundsException: Range [3, 3 + 2) out of bounds for length 4
//   skip-negative=0 pos-kept=104
//   mark-reset=101 101
//   after-close=104
//   own-2 first=104 avail=4 read3=3 [101, 108, 108] skip=1 then=-1 avail=0

import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.InputStream;
import java.util.Arrays;

public class L5W51ByteArrayInputStreamBodies {
    static void own(int round) {
        ByteArrayInputStream in = new ByteArrayInputStream("hello".getBytes());
        int first = in.read();
        int avail = in.available();
        byte[] b = new byte[3];
        int n = in.read(b, 0, 3);
        long skipped = in.skip(5);
        int then = in.read();
        int[] ints = new int[n];
        for (int i = 0; i < n; i++) {
            ints[i] = b[i];
        }
        System.out.println("own-" + round + " first=" + first + " avail=" + avail + " read3=" + n
                + " " + Arrays.toString(ints) + " skip=" + skipped + " then=" + then
                + " avail=" + in.available());
    }

    public static void main(String[] args) throws Exception {
        own(1);

        String self = L5W51ByteArrayInputStreamBodies.class.getName() + ".class";
        try (InputStream raw = L5W51ByteArrayInputStreamBodies.class.getResourceAsStream(self);
                DataInputStream data = new DataInputStream(raw)) {
            int magic = data.readInt();
            System.out.println("resource magic=" + Integer.toHexString(magic)
                    + " more=" + (data.read() >= 0));
        }

        ByteArrayInputStream in = new ByteArrayInputStream("hello".getBytes());
        try {
            in.read(new byte[4], 3, 2);
            System.out.println("range=ok");
        } catch (RuntimeException e) {
            System.out.println("range=" + e);
        }

        in = new ByteArrayInputStream("hello".getBytes());
        System.out.println("skip-negative=" + in.skip(-3) + " pos-kept=" + in.read());

        in = new ByteArrayInputStream("hello".getBytes());
        in.read();
        in.mark(10);
        int a = in.read();
        in.reset();
        System.out.println("mark-reset=" + a + " " + in.read());

        in = new ByteArrayInputStream("hello".getBytes());
        in.close();
        System.out.println("after-close=" + in.read());

        own(2);
    }
}
