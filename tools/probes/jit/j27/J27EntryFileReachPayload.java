import java.lang.ref.WeakReference;

public class J27EntryFileReachPayload {
    public static WeakReference<String> file() {
        return new WeakReference<>(new Throwable().getStackTrace()[0].getFileName());
    }
}
