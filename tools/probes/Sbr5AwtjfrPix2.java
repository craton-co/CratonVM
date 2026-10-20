import java.awt.*;
import java.awt.image.*;

/** SBR5 awtjfr: exhaustive destination-alpha sweep of the Porter-Duff loops (256 pixels per fill). */
public class Sbr5AwtjfrPix2 {
    public static void main(String[] a) {
        System.setProperty("java.awt.headless", "true");
        int type = a.length > 0 ? Integer.parseInt(a[0]) : BufferedImage.TYPE_INT_ARGB;
        int[] cols = {0x80d04020, 0x33ff0080, 0xf0102030};
        for (int rule = 1; rule <= 12; rule++)
            for (float ea : new float[]{1f, 0.5f})
                for (int c : cols) {
                    BufferedImage b = new BufferedImage(256, 1, type);
                    for (int i = 0; i < 256; i++) b.setRGB(i, 0, (i << 24) | ((i * 7) & 255) << 16 | ((i * 13 + 5) & 255) << 8 | ((i * 29 + 3) & 255));
                    Graphics2D g = b.createGraphics();
                    g.setComposite(AlphaComposite.getInstance(rule, ea));
                    g.setColor(new Color(c, true));
                    g.fillRect(0, 0, 256, 1);
                    g.dispose();
                    StringBuilder s = new StringBuilder();
                    for (int i = 0; i < 256; i++) s.append(Integer.toHexString(b.getRGB(i, 0))).append(' ');
                    System.out.println(rule + " " + ea + " " + Integer.toHexString(c) + " " + s);
                }
    }
}
