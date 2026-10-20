import java.awt.*;
import java.awt.image.*;

/** SBR5 awtjfr: single-pixel Porter-Duff differential (one fillRect per line). */
public class Sbr5AwtjfrPix {
    public static void main(String[] a) {
        System.setProperty("java.awt.headless", "true");
        int type = a.length > 0 ? Integer.parseInt(a[0]) : BufferedImage.TYPE_INT_ARGB;
        int[] dsts = {0x00000000, 0x80ff8040, 0xffc0a060, 0x40102030, 0xc0f0e0d0};
        int[] cols = {0x20d04020, 0xa0d04020, 0xffd04020, 0x01102030, 0xfe8090a0};
        for (int rule = 1; rule <= 12; rule++)
            for (float ea : new float[]{1f, 0.5f, 0.2f})
                for (int d : dsts)
                    for (int c : cols) {
                        BufferedImage b = new BufferedImage(1, 1, type);
                        b.setRGB(0, 0, d);
                        int before = b.getRGB(0, 0);
                        Graphics2D g = b.createGraphics();
                        g.setComposite(AlphaComposite.getInstance(rule, ea));
                        g.setColor(new Color(c, true));
                        g.fillRect(0, 0, 1, 1);
                        g.dispose();
                        System.out.println(rule + " " + ea + " " + Integer.toHexString(before) + " " + Integer.toHexString(c) + " " + Integer.toHexString(b.getRGB(0, 0)));
                    }
    }
}
