import java.awt.*;
import java.awt.image.*;
import java.io.*;
import javax.imageio.ImageIO;

/** SBR2 W7 AWT: BufferedImage / ImageIO / GraphicsEnvironment answers, diffed against HotSpot. */
public class Sbr2W7AwtImage {
    static void p(String s) { System.out.println(s); }
    static void t(String n, java.util.concurrent.Callable<Object> c) {
        try { p(n + " = " + c.call()); } catch (Throwable e) { p(n + " THROWS " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    public static void main(String[] a) throws Exception {
        System.setProperty("java.awt.headless", "true");
        BufferedImage bi = new BufferedImage(16, 8, BufferedImage.TYPE_INT_ARGB);
        p("w=" + bi.getWidth() + " h=" + bi.getHeight() + " type=" + bi.getType());
        bi.setRGB(3, 2, 0x80FF0040);
        bi.setRGB(15, 7, 0xFF112233);
        p("rgb=" + Integer.toHexString(bi.getRGB(3, 2)) + " " + Integer.toHexString(bi.getRGB(15, 7)) + " " + Integer.toHexString(bi.getRGB(0, 0)));
        t("raster", () -> bi.getRaster().getClass().getName() + " " + bi.getRaster().getWidth() + "x" + bi.getRaster().getHeight() + " bands=" + bi.getRaster().getNumBands());
        t("cm", () -> bi.getColorModel().getClass().getName() + " alpha=" + bi.getColorModel().hasAlpha());
        t("rasterpix", () -> { int[] px = new int[4]; bi.getRaster().getPixel(3, 2, px); return java.util.Arrays.toString(px); });
        t("rasterwrite", () -> { bi.getRaster().setPixel(5, 5, new int[]{1, 2, 3, 200}); return Integer.toHexString(bi.getRGB(5, 5)); });
        t("g2d", () -> {
            Graphics2D g = bi.createGraphics();
            g.setColor(Color.RED); g.fillRect(0, 0, 4, 4); g.dispose();
            return Integer.toHexString(bi.getRGB(1, 1));
        });
        BufferedImage rgb = new BufferedImage(4, 4, BufferedImage.TYPE_INT_RGB);
        rgb.setRGB(1, 1, 0x00123456);
        p("rgb-type getRGB=" + Integer.toHexString(rgb.getRGB(1, 1)) + " " + Integer.toHexString(rgb.getRGB(0, 0)));
        BufferedImage gray = new BufferedImage(4, 4, BufferedImage.TYPE_BYTE_GRAY);
        t("gray", () -> { gray.setRGB(1, 1, 0xFF808080); return gray.getType() + " " + Integer.toHexString(gray.getRGB(1, 1)); });
        BufferedImage bgr = new BufferedImage(4, 4, BufferedImage.TYPE_3BYTE_BGR);
        t("bgr", () -> { bgr.setRGB(2, 2, 0xFF102030); return bgr.getType() + " " + Integer.toHexString(bgr.getRGB(2, 2)); });
        t("oob", () -> bi.getRGB(99, 99));
        t("sub", () -> bi.getSubimage(2, 2, 4, 4).getWidth());
        t("props", () -> bi.getTransparency() + " " + bi.isAlphaPremultiplied());
        for (String fmt : new String[]{"png", "jpg", "bmp", "gif"}) {
            final String f = fmt;
            t("write-" + f, () -> {
                BufferedImage src = f.equals("jpg") || f.equals("bmp") ? rgbImage() : bi;
                ByteArrayOutputStream bo = new ByteArrayOutputStream();
                boolean ok = ImageIO.write(src, f, bo);
                byte[] b = bo.toByteArray();
                BufferedImage back = ok ? ImageIO.read(new ByteArrayInputStream(b)) : null;
                return ok + " magic=" + (b.length > 3 ? String.format("%02x%02x%02x", b[0], b[1], b[2]) : "-") + " back="
                    + (back == null ? "null" : back.getWidth() + "x" + back.getHeight() + " type=" + back.getType()
                       + " px=" + Integer.toHexString(back.getRGB(3, 2)));
            });
        }
        t("readers", () -> { String[] s = ImageIO.getReaderFormatNames(); java.util.Arrays.sort(s); return String.join(",", s); });
        t("writers", () -> { String[] s = ImageIO.getWriterFormatNames(); java.util.Arrays.sort(s); return String.join(",", s); });
        t("readnull", () -> ImageIO.read(new ByteArrayInputStream(new byte[]{1, 2, 3, 4})));
        t("readbad", () -> ImageIO.read((InputStream) null));
        p("headless=" + GraphicsEnvironment.isHeadless());
        t("ge", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().getClass().getName());
        t("ge-headless", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().isHeadlessInstance());
        t("ge-fonts", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().getAvailableFontFamilyNames().length > 0);
        t("ge-screens", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().getScreenDevices().length);
        t("ge-default", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().getDefaultScreenDevice());
        t("ge-g2d", () -> GraphicsEnvironment.getLocalGraphicsEnvironment().createGraphics(rgb) != null);
        t("toolkit", () -> Toolkit.getDefaultToolkit().getClass().getName());
        t("screensize", () -> Toolkit.getDefaultToolkit().getScreenSize());
        long t0 = System.nanoTime();
        int n = 50;
        BufferedImage big = new BufferedImage(256, 256, BufferedImage.TYPE_INT_RGB);
        for (int y = 0; y < 256; y++) for (int x = 0; x < 256; x++) big.setRGB(x, y, x * 65793 ^ y);
        for (int i = 0; i < n; i++) {
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            ImageIO.write(big, "png", bo);
            ImageIO.read(new ByteArrayInputStream(bo.toByteArray()));
        }
        System.err.println("PERF png256 roundtrip x" + n + " ms=" + (System.nanoTime() - t0) / 1000000);
    }
    static BufferedImage rgbImage() {
        BufferedImage r = new BufferedImage(16, 8, BufferedImage.TYPE_INT_RGB);
        r.setRGB(3, 2, 0x00FF0040);
        return r;
    }
}
