import java.lang.ref.WeakReference;

// The Class mirror holds the decoded filename, even after its Throwable dies.
// No literal here names the source filename: that would keep it interned alive.
public class J27EntryFileReach {
    static WeakReference<?>[] make() {
        Throwable trace = new Throwable();
        String file = trace.getStackTrace()[0].getFileName();
        return new WeakReference<?>[] { new WeakReference<>(file),
            new WeakReference<>(trace), new WeakReference<>(new Object()) };
    }
    public static void main(String[] args) throws Exception {
        WeakReference<?>[] refs = make();
        for (int i = 0; i < 8; i++) {
            byte[] pressure = new byte[1024 * 1024];
            System.gc();
            Thread.sleep(20);
        }
        System.out.println("file-alive " + (refs[0].get() != null));
        System.out.println("throwable-gone " + (refs[1].get() == null));
        System.out.println("control-gone " + (refs[2].get() == null));
    }
}
