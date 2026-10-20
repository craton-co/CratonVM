import java.awt.*;
import java.awt.geom.*;
import java.awt.image.*;
import java.util.zip.CRC32;

/**
 * SBR5 lane awtjfr: Java2D pixel differential. Every line is the CRC32 of
 * the whole image (as getRGB ARGB ints) after one operation, for every
 * standard BufferedImage type. Diff HotSpot against CratonVM line by line.
 */
public class Sbr5AwtjfrDraw {
    static final int[] TYPES = {
        BufferedImage.TYPE_INT_ARGB, BufferedImage.TYPE_INT_ARGB_PRE,
        BufferedImage.TYPE_INT_RGB, BufferedImage.TYPE_INT_BGR,
        BufferedImage.TYPE_3BYTE_BGR, BufferedImage.TYPE_4BYTE_ABGR,
        BufferedImage.TYPE_4BYTE_ABGR_PRE, BufferedImage.TYPE_BYTE_GRAY,
        BufferedImage.TYPE_USHORT_565_RGB, BufferedImage.TYPE_USHORT_555_RGB
    };
    static final String[] NAMES = {"ARGB", "ARGB_PRE", "RGB", "BGR", "3BYTE_BGR", "4BYTE_ABGR",
        "4BYTE_ABGR_PRE", "GRAY", "565", "555"};
    static long seed = 12345;
    static int rnd() { seed = seed * 6364136223846793005L + 1442695040888963407L; return (int) (seed >>> 33); }

    static String crc(BufferedImage b) {
        CRC32 c = new CRC32();
        for (int y = 0; y < b.getHeight(); y++)
            for (int x = 0; x < b.getWidth(); x++) {
                int p = b.getRGB(x, y);
                c.update(p >>> 24); c.update(p >>> 16); c.update(p >>> 8); c.update(p);
            }
        return Long.toHexString(c.getValue());
    }

    static BufferedImage mk(int type, int w, int h, boolean noise) {
        BufferedImage b = new BufferedImage(w, h, type);
        if (noise) {
            seed = 777 + type;
            for (int y = 0; y < h; y++) for (int x = 0; x < w; x++) b.setRGB(x, y, rnd() | ((type == BufferedImage.TYPE_INT_ARGB || type == BufferedImage.TYPE_INT_ARGB_PRE || type == 6 || type == 7) ? 0 : 0xff000000));
        }
        return b;
    }

    static void line(String s) { System.out.println(s); }

    static void run(String tag, java.util.concurrent.Callable<Object> c) {
        try { line(tag + " " + c.call()); } catch (Throwable e) { line(tag + " THROWS " + e.getClass().getName() + ": " + e.getMessage()); }
    }

    public static void main(String[] a) {
        System.setProperty("java.awt.headless", "true");
        for (int ti = 0; ti < TYPES.length; ti++) {
            final int type = TYPES[ti];
            final String n = NAMES[ti];
            run(n + " noise", () -> crc(mk(type, 20, 14, true)));
            run(n + " fillRect-opaque", () -> {
                BufferedImage b = mk(type, 20, 14, true); Graphics2D g = b.createGraphics();
                g.setColor(new Color(0x30, 0x90, 0xc0)); g.fillRect(3, 2, 9, 7); g.dispose(); return crc(b); });
            run(n + " fillRect-alpha", () -> {
                BufferedImage b = mk(type, 20, 14, true); Graphics2D g = b.createGraphics();
                g.setColor(new Color(0x30, 0x90, 0xc0, 0x80)); g.fillRect(3, 2, 9, 7); g.dispose(); return crc(b); });
            for (int rule = 1; rule <= 12; rule++) {
                final int r = rule;
                for (float ea : new float[]{1f, 0.5f, 0.3f}) {
                    final float e = ea;
                    run(n + " rule" + r + " ea" + e + " fill", () -> {
                        BufferedImage b = mk(type, 20, 14, true); Graphics2D g = b.createGraphics();
                        g.setComposite(AlphaComposite.getInstance(r, e));
                        g.setColor(new Color(0xd0, 0x40, 0x20, 0xa0)); g.fillRect(2, 2, 12, 9); g.dispose(); return crc(b); });
                }
            }
            run(n + " lines", () -> {
                BufferedImage b = mk(type, 20, 14, true); Graphics2D g = b.createGraphics();
                g.setColor(Color.WHITE); g.drawLine(0, 0, 19, 13); g.drawLine(5, 13, 15, 1); g.drawLine(2, 7, 18, 7); g.dispose(); return crc(b); });
            run(n + " drawRect", () -> {
                BufferedImage b = mk(type, 20, 14, true); Graphics2D g = b.createGraphics();
                g.setColor(Color.YELLOW); g.drawRect(2, 2, 10, 8); g.dispose(); return crc(b); });
            run(n + " fillOval", () -> {
                BufferedImage b = mk(type, 40, 30, true); Graphics2D g = b.createGraphics();
                g.setColor(new Color(10, 200, 100)); g.fillOval(3, 2, 33, 24); g.dispose(); return crc(b); });
            run(n + " drawOval", () -> {
                BufferedImage b = mk(type, 40, 30, true); Graphics2D g = b.createGraphics();
                g.setColor(new Color(10, 200, 100)); g.drawOval(3, 2, 33, 24); g.dispose(); return crc(b); });
            run(n + " fillPolygon", () -> {
                BufferedImage b = mk(type, 30, 30, true); Graphics2D g = b.createGraphics();
                g.setColor(Color.CYAN); g.fillPolygon(new int[]{3, 25, 15, 8}, new int[]{4, 8, 27, 20}, 4); g.dispose(); return crc(b); });
            run(n + " stroke3", () -> {
                BufferedImage b = mk(type, 40, 30, true); Graphics2D g = b.createGraphics();
                g.setColor(Color.MAGENTA); g.setStroke(new BasicStroke(3f)); g.drawLine(2, 3, 35, 20); g.draw(new Rectangle(5, 5, 20, 15)); g.dispose(); return crc(b); });
            run(n + " clipRect", () -> {
                BufferedImage b = mk(type, 20, 14, true); Graphics2D g = b.createGraphics();
                g.setClip(4, 3, 8, 6); g.setColor(Color.RED); g.fillRect(0, 0, 20, 14); g.dispose(); return crc(b); });
            run(n + " clipShape", () -> {
                BufferedImage b = mk(type, 30, 30, true); Graphics2D g = b.createGraphics();
                g.setClip(new Ellipse2D.Float(3, 3, 24, 20)); g.setColor(Color.ORANGE); g.fillRect(0, 0, 30, 30); g.dispose(); return crc(b); });
            run(n + " aa-oval", () -> {
                BufferedImage b = mk(type, 40, 30, true); Graphics2D g = b.createGraphics();
                g.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON);
                g.setColor(new Color(200, 20, 60)); g.fillOval(3, 2, 33, 24); g.dispose(); return crc(b); });
            run(n + " aa-line", () -> {
                BufferedImage b = mk(type, 40, 30, true); Graphics2D g = b.createGraphics();
                g.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON);
                g.setColor(new Color(20, 20, 60, 200)); g.setStroke(new BasicStroke(2.5f)); g.drawLine(2, 3, 35, 22); g.dispose(); return crc(b); });
            run(n + " translate-scale", () -> {
                BufferedImage b = mk(type, 40, 30, true); Graphics2D g = b.createGraphics();
                g.translate(3, 2); g.scale(1.5, 1.25); g.setColor(Color.GREEN); g.fillRect(1, 1, 10, 8); g.dispose(); return crc(b); });
            for (int si = 0; si < TYPES.length; si++) {
                final int st = TYPES[si]; final String sn = NAMES[si];
                run(n + " drawImage<-" + sn, () -> {
                    BufferedImage b = mk(type, 20, 14, true); BufferedImage s = mk(st, 9, 7, true);
                    Graphics2D g = b.createGraphics(); g.drawImage(s, 3, 2, null); g.dispose(); return crc(b); });
                run(n + " drawImage-half<-" + sn, () -> {
                    BufferedImage b = mk(type, 20, 14, true); BufferedImage s = mk(st, 9, 7, true);
                    Graphics2D g = b.createGraphics();
                    g.setComposite(AlphaComposite.getInstance(AlphaComposite.SRC_OVER, 0.5f));
                    g.drawImage(s, 3, 2, null); g.dispose(); return crc(b); });
            }
            run(n + " drawImage-scaled", () -> {
                BufferedImage b = mk(type, 30, 20, true); BufferedImage s = mk(BufferedImage.TYPE_INT_ARGB, 9, 7, true);
                Graphics2D g = b.createGraphics(); g.drawImage(s, 2, 2, 25, 15, null); g.dispose(); return crc(b); });
            run(n + " drawImage-bg", () -> {
                BufferedImage b = mk(type, 20, 14, true); BufferedImage s = mk(BufferedImage.TYPE_INT_ARGB, 9, 7, true);
                Graphics2D g = b.createGraphics(); g.drawImage(s, 3, 2, Color.BLUE, null); g.dispose(); return crc(b); });
            run(n + " drawImage-xform", () -> {
                BufferedImage b = mk(type, 30, 30, true); BufferedImage s = mk(BufferedImage.TYPE_INT_ARGB, 9, 7, true);
                Graphics2D g = b.createGraphics(); g.drawImage(s, AffineTransform.getRotateInstance(0.4, 12, 12), null); g.dispose(); return crc(b); });
            run(n + " xor", () -> {
                BufferedImage b = mk(type, 20, 14, true); Graphics2D g = b.createGraphics();
                g.setXORMode(Color.WHITE); g.setColor(Color.RED); g.fillRect(2, 2, 10, 8); g.drawLine(0, 0, 19, 13); g.dispose(); return crc(b); });
            run(n + " clearRect", () -> {
                BufferedImage b = mk(type, 20, 14, true); Graphics2D g = b.createGraphics();
                g.setBackground(new Color(40, 80, 120, 100)); g.clearRect(2, 2, 10, 8); g.dispose(); return crc(b); });
            run(n + " gradient", () -> {
                BufferedImage b = mk(type, 30, 20, true); Graphics2D g = b.createGraphics();
                g.setPaint(new GradientPaint(0, 0, Color.RED, 30, 20, new Color(0, 0, 255, 128)));
                g.fillRect(1, 1, 28, 18); g.dispose(); return crc(b); });
            run(n + " copyArea", () -> {
                BufferedImage b = mk(type, 20, 14, true); Graphics2D g = b.createGraphics();
                g.copyArea(0, 0, 8, 6, 5, 4); g.dispose(); return crc(b); });
            run(n + " subimage-raster", () -> {
                BufferedImage b = mk(type, 20, 14, true); BufferedImage s = b.getSubimage(2, 2, 10, 8);
                Graphics2D g = s.createGraphics(); g.setColor(Color.PINK); g.fillRect(1, 1, 5, 5); g.dispose(); return crc(b); });
        }
    }
}
