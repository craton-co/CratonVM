/**
 * zs s3/ob2: the launcher's GC thread flag handling, as HotSpot 25 does it.
 *
 * <p>The program itself only prints {@code started} (and {@code done}); the row
 * compares stdout, stderr and the exit status of the launch:
 * <ul>
 *   <li>a malformed {@code -XX:ConcGCThreads=} / {@code -XX:ParallelGCThreads=}
 *       value: nothing on stdout, HotSpot's three lines on stderr, exit 1;</li>
 *   <li>a HotSpot-valid non-decimal value ({@code 0x2}, {@code 1k} is too large
 *       for a real run, so {@code 0x2}): {@code started}/{@code done}, exit 0;</li>
 *   <li>a zero on the default collector: {@code started}/{@code done}, empty
 *       stderr, exit 0.</li>
 * </ul>
 */
public class Zs3Ob2ThreadFlags {
    public static void main(String[] args) {
        System.out.println("started");
        System.gc();
        System.out.println("done");
    }
}
