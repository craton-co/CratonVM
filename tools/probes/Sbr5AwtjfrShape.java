import java.awt.*;
import java.awt.geom.*;
import java.awt.image.*;

/** SBR5 awtjfr: ASCII dumps of non-AA / AA shape rasterisation, diffed against HotSpot. */
public class Sbr5AwtjfrShape {
    static void dump(String name, BufferedImage b) {
        System.out.println("== " + name);
        for (int y = 0; y < b.getHeight(); y++) {
            StringBuilder s = new StringBuilder();
            for (int x = 0; x < b.getWidth(); x++) {
                int a = b.getRGB(x, y) >>> 24;
                s.append(a == 0 ? '.' : a == 255 ? '#' : (char) ('0' + (a * 10 / 256)));
            }
            System.out.println(s);
        }
    }
    interface Op { void run(Graphics2D g); }
    static void t(String name, Op op, boolean aa) {
        BufferedImage b = new BufferedImage(48, 30, BufferedImage.TYPE_INT_ARGB);
        Graphics2D g = b.createGraphics();
        g.setColor(Color.WHITE);
        if (aa) g.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON);
        try { op.run(g); } catch (Throwable e) { System.out.println(name + " THROWS " + e); }
        g.dispose();
        dump(name, b);
    }
    public static void main(String[] a) {
        System.setProperty("java.awt.headless", "true");
        t("lines", g -> { g.drawLine(0, 0, 19, 13); g.drawLine(5, 13, 15, 1); g.drawLine(2, 7, 18, 7); g.drawLine(30, 2, 31, 28); g.drawLine(40, 29, 22, 20); }, false);
        t("fillOval", g -> g.fillOval(3, 2, 33, 24), false);
        t("drawOval", g -> g.drawOval(3, 2, 33, 24), false);
        t("fillRoundRect", g -> g.fillRoundRect(2, 2, 40, 24, 10, 10), false);
        t("fillPolygon", g -> g.fillPolygon(new int[]{3, 25, 15, 8}, new int[]{4, 8, 27, 20}, 4), false);
        t("stroke3", g -> { g.setStroke(new BasicStroke(3f)); g.drawLine(2, 3, 35, 20); g.draw(new Rectangle(5, 5, 20, 15)); }, false);
        t("translate-scale", g -> { g.translate(3, 2); g.scale(1.5, 1.25); g.fillRect(1, 1, 10, 8); }, false);
        t("arc", g -> g.fillArc(2, 2, 40, 25, 30, 200), false);
        t("aa-oval", g -> g.fillOval(3, 2, 33, 24), true);
        t("aa-line", g -> { g.setStroke(new BasicStroke(2.5f)); g.drawLine(2, 3, 35, 22); }, true);
        t("aa-rect", g -> g.fill(new Rectangle2D.Double(2.25, 3.5, 20.3, 11.7)), true);
        t("aa-poly", g -> g.fillPolygon(new int[]{3, 25, 15, 8}, new int[]{4, 8, 27, 20}, 4), true);
    }
}
