/**
 * gci i18/na: fill the heap to an OutOfMemoryError, keep it full, and drive
 * the class-initialisation doors that build a NativeContext OUTSIDE the native
 * call funnel (where the native allocators used to take their infallible arm
 * and abort the process):
 *
 * <ul>
 *   <li>{@code clinit}: under a running JFR recording, initialise twenty
 *       classes and one {@code jdk.jfr.Event} subclass for the first time.
 *       Each initialisation posts the JFR class-loaded event
 *       ({@code vm_util::jfr_event_class_initialized}) and the event subclass
 *       registers with the JFR backend, both through a context of their
 *       own.</li>
 * </ul>
 *
 * Stdout is deterministic (the number of classes tried, printed after the heap
 * is released); how many initialised and how many were refused goes to stderr
 * (HotSpot refuses some or none depending on how full the heap is: the class's
 * mirror, the JFR event). Any throwable counts as an answer; only an abort of
 * the process fails.
 */
public class GciNa18NativeDoorsProbe {
    static Object[] chain;

    static class C00 { static int v = 1; }
    static class C01 { static int v = 1; }
    static class C02 { static int v = 1; }
    static class C03 { static int v = 1; }
    static class C04 { static int v = 1; }
    static class C05 { static int v = 1; }
    static class C06 { static int v = 1; }
    static class C07 { static int v = 1; }
    static class C08 { static int v = 1; }
    static class C09 { static int v = 1; }
    static class C10 { static int v = 1; }
    static class C11 { static int v = 1; }
    static class C12 { static int v = 1; }
    static class C13 { static int v = 1; }
    static class C14 { static int v = 1; }
    static class C15 { static int v = 1; }
    static class C16 { static int v = 1; }
    static class C17 { static int v = 1; }
    static class C18 { static int v = 1; }
    static class C19 { static int v = 1; }

    static class Na18Event extends jdk.jfr.Event {
        static int v = 1;
        int x;
    }

    static final int CLASSES = 21;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "clinit";
        System.out.println("mode " + mode);
        if (mode.equals("clinit")) {
            clinit();
        } else {
            System.out.println("unknown mode");
        }
        System.out.println("done");
    }

    /** Fill the heap until an OutOfMemoryError, twice (large then small chunks). */
    static int fill() {
        Object[] head = null;
        int chunks = 0;
        try {
            while (true) {
                Object[] node = new Object[2];
                node[0] = head;
                node[1] = new long[4096];
                head = node;
                chunks++;
            }
        } catch (OutOfMemoryError e) {
            // keep what was allocated
        }
        chain = head;
        head = null;
        try {
            while (true) {
                Object[] node = new Object[2];
                node[0] = chain;
                chain = node;
                chunks++;
            }
        } catch (OutOfMemoryError e) {
            // the heap is full
        }
        return chunks;
    }

    static int touch(int i) {
        switch (i) {
            case 0: return C00.v;
            case 1: return C01.v;
            case 2: return C02.v;
            case 3: return C03.v;
            case 4: return C04.v;
            case 5: return C05.v;
            case 6: return C06.v;
            case 7: return C07.v;
            case 8: return C08.v;
            case 9: return C09.v;
            case 10: return C10.v;
            case 11: return C11.v;
            case 12: return C12.v;
            case 13: return C13.v;
            case 14: return C14.v;
            case 15: return C15.v;
            case 16: return C16.v;
            case 17: return C17.v;
            case 18: return C18.v;
            case 19: return C19.v;
            default: return Na18Event.v;
        }
    }

    static void clinit() throws Exception {
        jdk.jfr.Recording rec = new jdk.jfr.Recording();
        rec.enable("jdk.ClassLoad");
        rec.enable("jdk.ClassDefine");
        rec.start();
        int chunks = fill();
        int initialised = 0;
        int refused = 0;
        int tried = 0;
        for (int i = 0; i < CLASSES; i++) {
            tried++;
            try {
                initialised += touch(i);
            } catch (Throwable t) {
                refused++;
            }
        }
        chain = null;
        System.gc();
        rec.stop();
        rec.close();
        System.out.println("clinit full: tried " + tried);
        System.err.println("[na18] clinit chunks=" + chunks + " initialised=" + initialised + " refused=" + refused);
    }
}
