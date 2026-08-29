import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryMXBean;
import java.lang.management.MemoryManagerMXBean;
import java.lang.management.MemoryPoolMXBean;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;

/** Focused real-JDK regression probe for platform MXBean enumeration and the
 * inherited NotificationEmitterSupport state on MemoryImpl. */
public final class JmxProbe {
    public static void main(String[] args) throws Exception {
        MemoryPoolMXBean[] pools = ManagementFactory.getMemoryPoolMXBeans().toArray(new MemoryPoolMXBean[0]);
        MemoryManagerMXBean[] managers = ManagementFactory.getMemoryManagerMXBeans().toArray(new MemoryManagerMXBean[0]);
        GarbageCollectorMXBean[] collectors = ManagementFactory.getGarbageCollectorMXBeans().toArray(new GarbageCollectorMXBean[0]);
        System.out.println("pools=" + pools.length);
        System.out.println("mgrs=" + managers.length);
        System.out.println("gcs=" + collectors.length);

        MemoryMXBean memory = ManagementFactory.getMemoryMXBean();
        if (!(memory instanceof NotificationEmitter)) {
            throw new AssertionError("MemoryMXBean must be a NotificationEmitter, got " + memory.getClass());
        }
        NotificationEmitter emitter = (NotificationEmitter) memory;
        NotificationListener listener = new NotificationListener() {
            @Override public void handleNotification(Notification notification, Object handback) {}
        };
        emitter.addNotificationListener(listener, null, null);
        emitter.removeNotificationListener(listener);
        System.out.println("listener=OK");
        System.out.println("OK");
    }
}