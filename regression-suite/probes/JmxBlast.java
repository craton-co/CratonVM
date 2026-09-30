import java.lang.management.*;
public class JmxBlast {
    public static void main(String[] a) throws Exception {
        // Does the refusal take out things that never mention buffer pools?
        try { ManagementFactory.getPlatformMBeanServer(); System.out.println("getPlatformMBeanServer OK"); }
        catch (Throwable t) { System.out.println("getPlatformMBeanServer THREW " + t.getClass().getName()); }
        try { System.out.println("runtime name OK " + (ManagementFactory.getRuntimeMXBean().getName() != null)); }
        catch (Throwable t) { System.out.println("getRuntimeMXBean THREW " + t.getClass().getName()); }
        try { System.out.println("heap used OK " + (ManagementFactory.getMemoryMXBean().getHeapMemoryUsage() != null)); }
        catch (Throwable t) { System.out.println("getMemoryMXBean THREW " + t.getClass().getName()); }
    }
}
