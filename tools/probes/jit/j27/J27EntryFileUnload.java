import java.lang.ref.WeakReference;

// A decoded filename must not keep its defining class/loader alive.
public class J27EntryFileUnload {
    static class OwnedLoader extends ClassLoader {
        OwnedLoader() { super(null); }
        Class<?> payload(byte[] bytes) { return defineClass(null, bytes, 0, bytes.length); }
    }
    static WeakReference<?>[] make() throws Exception {
        byte[] bytes;
        try (var in = J27EntryFileUnload.class.getResourceAsStream("J27EntryFileReachPayload.class")) {
            bytes = in.readAllBytes();
        }
        OwnedLoader loader = new OwnedLoader();
        Class<?> payload = loader.payload(bytes);
        WeakReference<?> file = (WeakReference<?>) payload.getMethod("file").invoke(null);
        return new WeakReference<?>[] {file, new WeakReference<>(payload), new WeakReference<>(loader)};
    }
    public static void main(String[] args) throws Exception {
        WeakReference<?>[] refs = make();
        for (int i = 0; i < 64; i++) {
            System.gc();
            Thread.sleep(20);
        }
        System.out.println("file-gone " + (refs[0].get() == null));
        System.out.println("class-gone " + (refs[1].get() == null));
        System.out.println("loader-gone " + (refs[2].get() == null));
    }
}
