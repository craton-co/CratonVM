import java.io.File;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.List;
import java.util.Scanner;
import java.util.regex.Pattern;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;
import java.util.zip.ZipFile;

/**
 * Paired probe for the UNDER half of the layout-alias census
 * (W7-59-layout-detector-coverage.md §5.2, re-censused in
 * W7-68-live-under-allocations.md).
 *
 * <p><b>The trap this probe is built to avoid.</b> An under-allocated object is
 * NOT short — the base allocator clamps `slots = requested.max(declared)`
 * before it allocates, so the object comes back at the class's full declared
 * width every time the class is loaded. What the narrow request states is a
 * narrow SLOT MAP, and the damage is that slot <i>k</i> of that map lands on
 * whatever the real class declares at <i>k</i>. So "is the object non-null",
 * "did a value come back", and "does our own native read back what our own
 * native wrote" all pass against a completely mis-mapped object. That is the
 * vacuous shape this campaign keeps re-buying, and the index of vacuous greens
 * is full of it.
 *
 * <p>Every read below therefore goes through a <b>real JDK accessor</b> —
 * a method whose body is JDK bytecode reading the field by the JDK's own
 * declared index — and asserts an <b>exact value</b>, never a shape.
 *
 * <p>Run on HotSpot 25 first; every expected value in
 * W7-68-live-under-allocations.md is the transcript of that run, not a guess.
 *
 * <ol>
 *   <li><b>{@code FileChannel.isOpen()}</b> — REPAIRED by W7-68. The private
 *       slot map was {@code {0: fd, 1: position}} on a class declaring four
 *       fields, whose transitive order is
 *       {@code closeLock(0) closed(1) interruptor(2) interruptedTarget(3)}, all
 *       inherited from {@code AbstractInterruptibleChannel}. So the file
 *       position was the {@code closed} flag. {@code isOpen()} is
 *       {@code public final} on {@code AbstractInterruptibleChannel}, is real
 *       JDK bytecode returning {@code !closed}, and CratonVM registers no
 *       native on it for this class — so the read CANNOT be satisfied by the
 *       same wrong map that produced the write. Before the fix the channel
 *       reports itself CLOSED the moment the position moves off zero.</li>
 *   <li><b>{@code Path.toFile()}</b> — REPAIRED by W7-68. The old form
 *       allocated {@code java.io.File} one slot wide and wrote the path into
 *       slot 0. Slot 0 <i>is</i> {@code path}, so the shallow read passes; the
 *       object is still wrong, because {@code prefixLength} (slot 2) is left
 *       zero and {@code WinNTFileSystem.isAbsolute} is
 *       {@code (pl == 2 &amp;&amp; charAt(0) == slash) || pl == 3}. All of
 *       {@code isAbsolute}, {@code getAbsolutePath} and {@code toPath} are real
 *       JDK bytecode over that field.</li>
 *   <li><b>{@code MappedByteBuffer}</b> — NOT repaired; printed so the next
 *       lane has its red. The private slots sit at 10 and 11 on a class
 *       declaring 13, i.e. on {@code nativeByteOrder} and {@code fd}.</li>
 *   <li><b>{@code Scanner.delimiter()}</b> — the {@code Pattern} 2-vs-20 row.
 *       Exercises the object through {@code pattern()}, {@code flags()} and a
 *       real {@code matcher(...).find()}, which is what a truncated
 *       {@code Pattern} cannot survive.</li>
 *   <li><b>{@code ZipEntry} getters</b> — the 6-vs-14 row.</li>
 *   <li><b>{@code Files.readAllLines}</b> — the {@code ArrayList} 2-vs-3 row,
 *       driven through real {@code List} bytecode.</li>
 * </ol>
 */
public class UnderAllocationProbe {

    static int failures = 0;

    static void check(String what, Object expected, Object actual) {
        boolean ok = expected == null ? actual == null : expected.equals(actual);
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "  ok   " : "  FAIL ") + what + " = " + stable(actual)
                + (ok ? "" : "   (expected " + stable(expected) + ")"));
    }

    /**
     * Erases the random component of a temp-file name from RENDERED output.
     *
     * `Files.createTempFile("underalloc", ...)` picks a fresh number every run,
     * so three of this probe's lines carried a value that differs between any
     * two runs of any two VMs. Diffing HotSpot against CratonVM then reports
     * three divergences that are the FILENAME, and a reader has to know which
     * three to ignore -- the same defect the HttpServer wildcard probe had with
     * ephemeral ports.
     *
     * The COMPARISON is untouched: `check` still tests the real values, which
     * both come from the same run and so still have to be equal. Only the
     * printed form is normalised, so a genuine mismatch still shows as FAIL and
     * still prints both sides.
     */
    static Object stable(Object o) {
        if (o == null) {
            return null;
        }
        String s = String.valueOf(o);
        int i = s.indexOf("underalloc");
        if (i < 0) {
            return o;
        }
        int j = i + "underalloc".length();
        int k = j;
        while (k < s.length() && Character.isDigit(s.charAt(k))) {
            k++;
        }
        return k == j ? o : s.substring(0, j) + "<n>" + s.substring(k);
    }

    static String show(ThrowingSupplier<?> s) {
        try {
            return String.valueOf(s.get());
        } catch (Throwable t) {
            return t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
    }

    interface ThrowingSupplier<T> {
        T get() throws Throwable;
    }

    public static void main(String[] args) throws Exception {
        fileChannelIsOpenSection();
        pathToFileSection();
        mappedByteBufferSection();
        scannerPatternSection();
        zipEntrySection();
        readAllLinesSection();
        System.out.println();
        System.out.println("failures=" + failures);
    }

    // -- 1 ---------------------------------------------------------------
    // FileChannel.isOpen() is AbstractInterruptibleChannel bytecode reading
    // `closed`, which the old 2-slot map used for the file position.
    static void fileChannelIsOpenSection() throws Exception {
        System.out.println("[1] FileChannel.isOpen() across a read that moves the position");
        Path p = Files.createTempFile("underalloc", ".txt");
        Files.writeString(p, "abcdefghij");
        try (FileChannel ch = FileChannel.open(p, StandardOpenOption.READ)) {
            check("isOpen() before any read", Boolean.TRUE, ch.isOpen());
            ByteBuffer bb = ByteBuffer.allocate(4);
            int n = ch.read(bb);
            check("read(4)", Integer.valueOf(4), Integer.valueOf(n));
            // THE read that cannot be faked: `closed` by the JDK's own index.
            check("isOpen() after the position moved to 4", Boolean.TRUE, ch.isOpen());
            check("position() after the read", Long.valueOf(4L), Long.valueOf(ch.position()));
            ch.position(7L);
            check("isOpen() after position(7)", Boolean.TRUE, ch.isOpen());
            check("position() after position(7)", Long.valueOf(7L), Long.valueOf(ch.position()));
        }
        Files.deleteIfExists(p);
        System.out.println();
    }

    // -- 2 ---------------------------------------------------------------
    // Path.toFile() -> java.io.File. `prefixLength` is slot 2 of four.
    static void pathToFileSection() throws Exception {
        System.out.println("[2] Path.toFile() -> real java.io.File accessors");
        Path abs = Files.createTempFile("underalloc", ".txt").toAbsolutePath();
        File f = abs.toFile();
        check("getPath().equals(the path)", abs.toString(), f.getPath());
        // isAbsolute() is WinNTFileSystem/UnixFileSystem bytecode over
        // `prefixLength`, which a one-slot fabrication never sets.
        check("isAbsolute() on an absolute path", Boolean.TRUE, Boolean.valueOf(f.isAbsolute()));
        // getAbsolutePath() resolves through the same field.
        check("getAbsolutePath()", abs.toString(), f.getAbsolutePath());
        check("toPath() round-trips", abs, f.toPath());
        check("exists()", Boolean.TRUE, Boolean.valueOf(f.exists()));
        File rel = new File("relative-name.txt");
        check("isAbsolute() on a relative name", Boolean.FALSE, Boolean.valueOf(rel.isAbsolute()));
        Files.deleteIfExists(abs);
        System.out.println();
    }

    // -- 3 ---------------------------------------------------------------
    // NOT repaired. Printed, not asserted, so the row is a measurement and
    // not a green this lane did not earn.
    static void mappedByteBufferSection() throws Exception {
        System.out.println("[3] MappedByteBuffer (NOT repaired -- private slots 10/11 alias "
                + "nativeByteOrder/fd on a class declaring 13)");
        Path p = Files.createTempFile("underalloc", ".bin");
        Files.write(p, new byte[] {1, 2, 3, 4, 5, 6, 7, 8});
        try (FileChannel ch = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            MappedByteBuffer mbb = ch.map(FileChannel.MapMode.READ_WRITE, 0, 8);
            System.out.println("  capacity()      = " + show(() -> mbb.capacity()));
            System.out.println("  get(0)          = " + show(() -> mbb.get(0)));
            System.out.println("  isLoaded()      = " + show(() -> mbb.isLoaded()));
            System.out.println("  force()         = " + show(() -> mbb.force() != null));
            // The read that reaches `fd` through real JDK bytecode: force(int,int)
            // is NOT registered by CratonVM, unlike the no-arg force().
            System.out.println("  force(0,8)      = " + show(() -> mbb.force(0, 8) != null));
            System.out.println("  isReadOnly()    = " + show(() -> mbb.isReadOnly()));
        }
        Files.deleteIfExists(p);
        System.out.println();
    }

    // -- 4 ---------------------------------------------------------------
    static void scannerPatternSection() {
        System.out.println("[4] Scanner.delimiter() -> java.util.regex.Pattern (2 vs 20)");
        Scanner sc = new Scanner("x,y,z");
        sc.useDelimiter(",");
        Pattern d = sc.delimiter();
        // pattern() and flags() are real Pattern bytecode over slots 0 and 1 --
        // which the 2-slot map happens to get right. They are printed BECAUSE
        // they pass against a truncated object: they are the vacuous read.
        System.out.println("  delimiter().pattern() = " + show(() -> d.pattern()));
        System.out.println("  delimiter().flags()   = " + show(() -> d.flags()));
        // THIS is the read that cannot be faked: matcher() compiles lazily off
        // `compiled`(3)/`root`(5)/`capturingGroupCount`(15)/`localCount`(16).
        System.out.println("  delimiter().matcher(\"x,y\").find() = "
                + show(() -> d.matcher("x,y").find()));
        System.out.println("  scanner next()        = " + show(() -> sc.next()));
        System.out.println();
    }

    // -- 5 ---------------------------------------------------------------
    static void zipEntrySection() throws Exception {
        System.out.println("[5] ZipEntry getters (6 vs 14)");
        Path zip = Files.createTempFile("underalloc", ".zip");
        try (ZipOutputStream zos = new ZipOutputStream(Files.newOutputStream(zip))) {
            ZipEntry e = new ZipEntry("hello.txt");
            zos.putNextEntry(e);
            zos.write("hello".getBytes("UTF-8"));
            zos.closeEntry();
        }
        try (ZipFile zf = new ZipFile(zip.toFile())) {
            ZipEntry e = zf.getEntry("hello.txt");
            check("getName()", "hello.txt", e.getName());
            check("getMethod() is DEFLATED", Integer.valueOf(ZipEntry.DEFLATED),
                    Integer.valueOf(e.getMethod()));
            check("getSize()", Long.valueOf(5L), Long.valueOf(e.getSize()));
            check("isDirectory()", Boolean.FALSE, Boolean.valueOf(e.isDirectory()));
            // getTime() is xdostime(1)/mtime(2) bytecode. A 1979 answer is the
            // signature of `method` written into slot 1.
            long t = e.getTime();
            boolean modern = t > 946684800000L; // 2000-01-01
            check("getTime() is after 2000-01-01", Boolean.TRUE, Boolean.valueOf(modern));
        }
        Files.deleteIfExists(zip);
        System.out.println();
    }

    // -- 6 ---------------------------------------------------------------
    static void readAllLinesSection() throws Exception {
        System.out.println("[6] Files.readAllLines -> java.util.ArrayList (2 vs 3)");
        Path p = Files.createTempFile("underalloc", ".txt");
        Files.writeString(p, "alpha\nbeta\ngamma\n");
        List<String> lines = Files.readAllLines(p);
        check("size()", Integer.valueOf(3), Integer.valueOf(lines.size()));
        check("get(1)", "beta", lines.get(1));
        StringBuilder sb = new StringBuilder();
        for (String s : lines) {          // AbstractList.Itr -- reads modCount
            sb.append(s).append('|');
        }
        check("enhanced-for join", "alpha|beta|gamma|", sb.toString());
        check("indexOf(gamma)", Integer.valueOf(2), Integer.valueOf(lines.indexOf("gamma")));
        // A copy through a REAL ArrayList constructor, which reads size and
        // elementData by the JDK's own indices.
        List<String> copy = new ArrayList<>(lines);
        check("new ArrayList<>(lines).size()", Integer.valueOf(3), Integer.valueOf(copy.size()));
        check("copy.equals(lines)", Boolean.TRUE, Boolean.valueOf(copy.equals(lines)));
        Files.deleteIfExists(p);
        System.out.println();
    }
}
