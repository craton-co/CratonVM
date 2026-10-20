import java.awt.*;
import java.awt.image.BufferedImage;
import java.lang.management.*;
import java.util.concurrent.Callable;

/**
 * SBR3 lane awt: headless Toolkit / GraphicsEnvironment / ManagementFactory answers,
 * diffed against HotSpot 25. Run with -Djava.awt.headless=true.
 */
public class Sbr3AwtGe {
    static void p(String s) { System.out.println(s); }
    static void t(String n, Callable<Object> c) {
        try { p(n + " = " + c.call()); } catch (Throwable e) { p(n + " THROWS " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    public static void main(String[] a) throws Exception {
        t("isHeadless", () -> GraphicsEnvironment.isHeadless());
        t("toolkit", () -> Toolkit.getDefaultToolkit().getClass().getName());
        t("toolkit-same", () -> Toolkit.getDefaultToolkit() == Toolkit.getDefaultToolkit());
        t("screensize", () -> Toolkit.getDefaultToolkit().getScreenSize());
        t("beep", () -> { Toolkit.getDefaultToolkit().beep(); return "ok"; });
        t("ge", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().getClass().getName());
        t("ge-same", () -> GraphicsEnvironment.getLocalGraphicsEnvironment() == GraphicsEnvironment.getLocalGraphicsEnvironment());
        t("ge-headless", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().isHeadlessInstance());
        t("ge-fonts", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().getAvailableFontFamilyNames().length >= 5);
        t("ge-screens", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().getScreenDevices());
        t("ge-default", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().getDefaultScreenDevice());
        t("ge-g2d", () -> {
            BufferedImage bi = new BufferedImage(4, 4, BufferedImage.TYPE_INT_ARGB);
            Graphics2D g = GraphicsEnvironment.getLocalGraphicsEnvironment().createGraphics(bi);
            g.setColor(Color.RED); g.fillRect(0, 0, 4, 4); g.dispose();
            return Integer.toHexString(bi.getRGB(1, 1));
        });
        t("font", () -> new Font("Dialog", Font.PLAIN, 12).getFamily());
        t("mx-runtime", () -> ManagementFactory.getRuntimeMXBean().getClass().getSimpleName());
        t("mx-mem", () -> ManagementFactory.getMemoryMXBean().getHeapMemoryUsage().getUsed() > 0);
        t("mx-thread", () -> ManagementFactory.getThreadMXBean().getThreadCount() > 0);
        t("mx-os", () -> ManagementFactory.getOperatingSystemMXBean().getAvailableProcessors() > 0);
        t("mx-server", () -> ManagementFactory.getPlatformMBeanServer().isRegistered(new javax.management.ObjectName("java.lang:type=Runtime")));
        t("mx-class", () -> ManagementFactory.getClassLoadingMXBean().getLoadedClassCount() > 0);
    }
}
