import java.io.*;

/**
 * What every operation on a CLOSED {@link FileInputStream} / {@link
 * FileOutputStream} answers on the reference JDK.
 *
 * This is the oracle for `native-io`'s ten `None if f{i,o}s_is_closed` arms.
 * Those arms already carry "(measured)" comments; this re-takes the whole
 * matrix in one run so the ordering fix is checked against the platform rather
 * than against the comments, and so the ZERO-LENGTH carve-outs — which are the
 * rows most likely to be got wrong by a fix that hoists the closed check — are
 * measured beside the rows they are exceptions to.
 *
 * Each line is `op -> value` or `op -> THREW <class>: <message>`.
 */
public final class ClosedStreamOracle {

    private static void row(String op, Callable c) {
        String out;
        try {
            Object v = c.call();
            out = String.valueOf(v);
        } catch (Throwable t) {
            out = "THREW " + t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.printf("@@ROW %-34s -> %s%n", op, out);
    }

    private interface Callable {
        Object call() throws Exception;
    }

    public static void main(String[] args) throws Exception {
        File f = File.createTempFile("closed-oracle", ".bin");
        f.deleteOnExit();
        try (FileOutputStream seed = new FileOutputStream(f)) {
            seed.write(new byte[] { 1, 2, 3, 4, 5, 6, 7, 8 });
        }

        // ---- FileInputStream, closed ----
        final FileInputStream in = new FileInputStream(f);
        in.close();
        final byte[] b4 = new byte[4];
        final byte[] b0 = new byte[0];

        row("fis.read()", in::read);
        row("fis.read(byte[4])", () -> in.read(b4));
        row("fis.read(byte[0])", () -> in.read(b0));
        row("fis.read(b4, 0, 4)", () -> in.read(b4, 0, 4));
        row("fis.read(b4, 0, 0)", () -> in.read(b4, 0, 0));
        row("fis.available()", in::available);
        row("fis.skip(0)", () -> in.skip(0));
        row("fis.skip(-5)", () -> in.skip(-5));
        row("fis.skip(1)", () -> in.skip(1));
        row("fis.close() [double]", () -> { in.close(); return "void"; });
        row("fis.getChannel().isOpen()", () -> in.getChannel().isOpen());

        // ---- FileOutputStream, closed ----
        final FileOutputStream out = new FileOutputStream(f, true);
        out.close();

        row("fos.write(int)", () -> { out.write(7); return "void"; });
        row("fos.write(byte[4])", () -> { out.write(b4); return "void"; });
        row("fos.write(byte[0])", () -> { out.write(b0); return "void"; });
        row("fos.write(b4, 0, 4)", () -> { out.write(b4, 0, 4); return "void"; });
        row("fos.write(b4, 0, 0)", () -> { out.write(b4, 0, 0); return "void"; });
        row("fos.flush()", () -> { out.flush(); return "void"; });
        row("fos.close() [double]", () -> { out.close(); return "void"; });

        // ---- the ORDER question: does a bad argument outrank the closed state? ----
        // The fix hoists the closed check above the descriptor lookup; these
        // rows say whether it may also be hoisted above the bounds checks.
        row("fis.read(b4, -1, 1) closed", () -> in.read(b4, -1, 1));
        row("fis.read(b4, 0, 99) closed", () -> in.read(b4, 0, 99));
        row("fos.write(b4, -1, 1) closed", () -> { out.write(b4, -1, 1); return "void"; });
        row("fos.write(b4, 0, 99) closed", () -> { out.write(b4, 0, 99); return "void"; });
        row("fis.read(null, 0, 1) closed", () -> in.read(null, 0, 1));
        row("fos.write(null, 0, 1) closed", () -> { out.write((byte[]) null, 0, 1); return "void"; });

        System.out.println("@@ORACLE_DONE");
    }
}
