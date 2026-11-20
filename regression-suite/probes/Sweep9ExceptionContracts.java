import java.nio.*;
import java.nio.charset.*;
import java.util.concurrent.*;
import java.util.concurrent.locks.*;

/** Sweep 9: exception contracts outside reflection — threads, locks, loading, charsets. */
public class Sweep9ExceptionContracts {
    interface Call { void run() throws Exception; }
    static void t(String l, Call c) {
        try { c.run(); System.out.println("Y " + l + " = no throw"); }
        catch (Throwable x) {
            System.out.println("Y " + l + " = " + x.getClass().getName() + " | " + x.getMessage());
        }
    }

    public static void main(String[] a) throws Exception {
        // ---- interrupt / join / sleep ----------------------------------------
        t("sleep_negative", () -> Thread.sleep(-1));
        t("join_negative", () -> new Thread().join(-1));
        t("wait_no_monitor", () -> { Object o = new Object(); o.wait(); });
        t("notify_no_monitor", () -> { Object o = new Object(); o.notify(); });
        t("wait_negative", () -> { Object o = new Object(); synchronized (o) { o.wait(-5); } });
        t("start_twice", () -> { Thread th = new Thread(() -> {}); th.start(); th.start(); });
        t("setPriority_bad", () -> new Thread().setPriority(99));
        t("interrupted_flag", () -> {
            Thread.currentThread().interrupt();
            boolean first = Thread.interrupted();
            boolean second = Thread.interrupted();
            if (!first || second) throw new AssertionError("first=" + first + " second=" + second);
        });
        t("sleep_after_interrupt", () -> {
            Thread.currentThread().interrupt();
            try { Thread.sleep(1); } finally {
                if (Thread.interrupted()) throw new AssertionError("flag not cleared by throw");
            }
        });

        // ---- locks and conditions --------------------------------------------
        t("unlock_not_held", () -> new ReentrantLock().unlock());
        t("await_not_held", () -> new ReentrantLock().newCondition().await());
        t("signal_not_held", () -> new ReentrantLock().newCondition().signal());
        t("rwlock_write_unheld", () -> new ReentrantReadWriteLock().writeLock().unlock());
        t("lock_getHoldCount", () -> {
            ReentrantLock l = new ReentrantLock();
            l.lock(); l.lock();
            int n = l.getHoldCount();
            l.unlock(); l.unlock();
            if (n != 2) throw new AssertionError("holdCount=" + n);
        });
        t("semaphore_negative", () -> new Semaphore(1).acquire(-1));
        t("cdl_negative", () -> new CountDownLatch(-1));
        t("cyclic_zero", () -> new CyclicBarrier(0));

        // ---- class loading and resources -------------------------------------
        t("forName_missing", () -> Class.forName("no.such.Klass"));
        t("forName_null", () -> Class.forName(null));
        t("forName_array_binary", () -> Class.forName("[I"));
        t("forName_primitive", () -> Class.forName("int"));
        t("loadClass_missing", () -> Sweep9ExceptionContracts.class.getClassLoader().loadClass("no.such.Klass"));
        t("getResource_missing", () -> {
            if (Sweep9ExceptionContracts.class.getResource("/no/such/thing.txt") != null) throw new AssertionError("found");
        });
        t("getResourceAsStream_missing", () -> {
            if (Sweep9ExceptionContracts.class.getResourceAsStream("/no/such/thing.txt") != null)
                throw new AssertionError("found");
        });

        // ---- charset decode / encode error actions ----------------------------
        byte[] bad = new byte[] {(byte) 0xC3, (byte) 0x28};       // invalid UTF-8
        t("decode_REPORT", () -> StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(bad)));
        t("decode_REPLACE", () -> {
            String s = StandardCharsets.UTF_8.newDecoder()
                    .onMalformedInput(CodingErrorAction.REPLACE)
                    .decode(ByteBuffer.wrap(bad)).toString();
            if (s.indexOf('�') < 0) throw new AssertionError("no replacement: " + s.length());
        });
        t("encode_unmappable_REPORT", () -> Charset.forName("US-ASCII").newEncoder()
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .encode(CharBuffer.wrap("café")));
        t("encode_lone_surrogate", () -> Charset.forName("UTF-8").newEncoder()
                .encode(CharBuffer.wrap("a" + '\ud800' + "b")));
        t("charset_unsupported", () -> Charset.forName("no-such-charset-42"));
        t("charset_illegal_name", () -> Charset.forName("bad name!"));
        t("string_bad_charset", () -> "x".getBytes("no-such-charset-42"));
        t("decoder_malformed_len", () -> {
            try {
                StandardCharsets.UTF_8.newDecoder()
                        .onMalformedInput(CodingErrorAction.REPORT)
                        .decode(ByteBuffer.wrap(bad));
            } catch (MalformedInputException e) {
                throw new IllegalStateException("len=" + e.getInputLength(), e);
            }
        });
    }
}
