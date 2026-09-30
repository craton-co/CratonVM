import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;

/**
 * `System.in`, with stdin ACTUALLY FED.
 *
 * `WORKER-4-1` N4 records that `System.in` is a `java.io.FileInputStream` on
 * this VM where HotSpot installs a `java.io.BufferedInputStream`, and refuses
 * the fix because `lang_system::native_system_init_phase1` builds a
 * `FileInputStream`-SHAPED carrier whose fd lives in a SLOT rather than behind
 * a working `read` — so wrapping it could produce a `System.in` that reads
 * nothing.
 *
 * That refusal was reasoned, not measured, and no probe in this lane had ever
 * fed stdin: every run so far saw an empty stream, where "reads nothing" and
 * "works correctly" are the same observation. This probe is run with input on
 * stdin, so the two are distinguishable.
 *
 * The question it settles is narrow and mechanical: **can the carrier be read
 * through the ordinary `InputStream` API, or only through the slot?** If
 * `System.in.read()` and `new BufferedInputStream(System.in).read()` both
 * deliver the bytes, the wrap is safe and N4 is a one-line fix. If only the
 * bare carrier delivers them, N4 stays refused and this records why.
 */
public class W4Stdin {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    interface Thunk { Object call() throws Exception; }

    static void ckT(String tag, Thunk t) {
        try {
            ck(tag, t.call());
        } catch (Throwable e) {
            ck(tag, "threw:" + e.getClass().getName());
        }
    }

    public static void main(String[] args) throws Exception {
        // Identity first, for the record.
        ck("class", System.in.getClass().getName());
        ck("isBuffered", System.in instanceof BufferedInputStream);
        ck("isFileInputStream", System.in instanceof FileInputStream);
        ck("markSupported", System.in.markSupported());

        // The mode is chosen by argv so one binary can answer each question in
        // its own process — stdin is consumed by whichever reader goes first,
        // so they cannot share a run.
        String mode = args.length > 0 ? args[0] : "raw";
        switch (mode) {
            case "raw" -> {
                // Straight off the carrier: the path that works today.
                ck("raw.read1", System.in.read());
                byte[] b = new byte[8];
                int n = System.in.read(b, 0, 8);
                ck("raw.read3.n", n);
                ck("raw.read3.content", n > 0 ? new String(b, 0, n, StandardCharsets.UTF_8) : "<none>");
                ck("raw.readAllBytes",
                        new String(System.in.readAllBytes(), StandardCharsets.UTF_8).trim());
            }
            case "buffered" -> {
                // THE QUESTION. If this delivers the same bytes, `System.in`
                // can be wrapped the way HotSpot wraps it.
                BufferedInputStream in = new BufferedInputStream(System.in);
                ck("buffered.markSupported", in.markSupported());
                ck("buffered.read1", in.read());
                byte[] b = new byte[8];
                int n = in.read(b, 0, 8);
                ck("buffered.read3.n", n);
                ck("buffered.read3.content", n > 0 ? new String(b, 0, n, StandardCharsets.UTF_8) : "<none>");
                ck("buffered.readAllBytes",
                        new String(in.readAllBytes(), StandardCharsets.UTF_8).trim());
            }
            case "scanner" -> {
                // The consumer the current shape exists for.
                Scanner sc = new Scanner(System.in);
                ck("scanner.hasNext", sc.hasNext());
                ck("scanner.next", sc.next());
                ck("scanner.nextLineRest", sc.hasNextLine() ? sc.nextLine() : "<no line>");
                ck("scanner.nextLine2", sc.hasNextLine() ? sc.nextLine() : "<no line>");
            }
            case "scannerBuffered" -> {
                // A Scanner over a WRAPPED System.in, which is what the wrap
                // would make the default.
                Scanner sc = new Scanner(new BufferedInputStream(System.in));
                ck("scannerBuffered.hasNext", sc.hasNext());
                ck("scannerBuffered.next", sc.next());
            }
            case "reader" -> {
                BufferedReader r = new BufferedReader(
                        new InputStreamReader(System.in, StandardCharsets.UTF_8));
                ck("reader.line1", r.readLine());
                ck("reader.line2", r.readLine());
                ck("reader.line3", r.readLine());
            }
            default -> ck("mode.unknown", mode);
        }

        System.out.println("PASS W4Stdin");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
