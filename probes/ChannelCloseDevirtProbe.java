import java.nio.channels.*;
import java.nio.channels.spi.SelectorProvider;

/**
 * netty's NioDatagramChannel.doClose() shape: `javaChannel().close()` through a
 * small delegating method, called often enough for the JIT to compile it.
 * `AbstractInterruptibleChannel.close()` is `public final`, so the final-method
 * devirtualiser can bind the site straight to the JDK body — which opens with
 * `synchronized (closeLock)` on a field CratonVM's channel factories may never
 * have seeded.
 */
public class ChannelCloseDevirtProbe {
    static void doCloseLikeNetty(SelectableChannel ch) throws Exception { ch.close(); }

    public static void main(String[] a) throws Exception {
        SelectorProvider p = SelectorProvider.provider();
        int reps = Integer.getInteger("reps", 4000);
        int failures = 0, firstFailAt = -1;
        String firstMsg = null;
        int stillOpen = 0, firstStillOpenAt = -1;
        for (int i = 0; i < reps; i++) {
            DatagramChannel d;
            try {
                d = p.openDatagramChannel();
            } catch (Throwable openFail) {
                System.out.println("openDatagramChannel FAILED at i=" + i + ": " + openFail);
                break;
            }
            try {
                doCloseLikeNetty(d);
            } catch (Throwable t) {
                failures++;
                if (firstFailAt < 0) { firstFailAt = i; firstMsg = t.toString(); }
            }
            if (d.isOpen()) { stillOpen++; if (firstStillOpenAt < 0) { firstStillOpenAt = i; } }
        }
        System.out.println("datagram close over " + reps + ": threw=" + failures
                + " firstFailAt=" + firstFailAt + " first=" + firstMsg
                + " | stillOpenAfterClose=" + stillOpen + " firstAt=" + firstStillOpenAt);

        failures = 0; firstFailAt = -1; firstMsg = null; stillOpen = 0; firstStillOpenAt = -1;
        for (int i = 0; i < reps; i++) {
            SocketChannel s;
            try {
                s = p.openSocketChannel();
            } catch (Throwable openFail) {
                System.out.println("openSocketChannel FAILED at i=" + i + ": " + openFail);
                break;
            }
            try {
                doCloseLikeNetty(s);
            } catch (Throwable t) {
                failures++;
                if (firstFailAt < 0) { firstFailAt = i; firstMsg = t.toString(); }
            }
            if (s.isOpen()) { stillOpen++; if (firstStillOpenAt < 0) { firstStillOpenAt = i; } }
        }
        System.out.println("socket   close over " + reps + ": threw=" + failures
                + " firstFailAt=" + firstFailAt + " first=" + firstMsg
                + " | stillOpenAfterClose=" + stillOpen + " firstAt=" + firstStillOpenAt);
    }
}
