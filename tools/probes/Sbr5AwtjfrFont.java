import java.awt.*;
import java.awt.font.*;
import java.awt.image.*;
import java.util.zip.CRC32;

/** SBR5 awtjfr: font families, metrics and text rendering, diffed against HotSpot. */
public class Sbr5AwtjfrFont {
    static String crc(BufferedImage b) {
        CRC32 c = new CRC32();
        for (int y = 0; y < b.getHeight(); y++) for (int x = 0; x < b.getWidth(); x++) { int p = b.getRGB(x, y); c.update(p >>> 24); c.update(p >>> 16); c.update(p >>> 8); c.update(p); }
        return Long.toHexString(c.getValue());
    }
    public static void main(String[] a) {
        System.setProperty("java.awt.headless", "true");
        GraphicsEnvironment ge = GraphicsEnvironment.getLocalGraphicsEnvironment();
        System.out.println("families " + String.join(",", ge.getAvailableFontFamilyNames()));
        BufferedImage img = new BufferedImage(10, 10, BufferedImage.TYPE_INT_ARGB);
        Graphics2D g = img.createGraphics();
        for (String fam : new String[]{"Dialog", "SansSerif", "Serif", "Monospaced", "DejaVu Sans", "DejaVu Serif", "DejaVu Sans Mono"}) {
            for (int style : new int[]{Font.PLAIN, Font.BOLD, Font.ITALIC}) {
                for (int size : new int[]{9, 12, 13, 18, 24, 40}) {
                    Font f = new Font(fam, style, size);
                    FontMetrics m = g.getFontMetrics(f);
                    System.out.println("fm " + fam + " " + style + " " + size + " family=" + f.getFamily() + " name=" + f.getFontName()
                        + " asc=" + m.getAscent() + " desc=" + m.getDescent() + " lead=" + m.getLeading() + " h=" + m.getHeight()
                        + " max=" + m.getMaxAdvance() + " w=" + m.stringWidth("The quick brown fox 0123") + " W=" + m.charWidth('W') + " i=" + m.charWidth('i'));
                }
            }
        }
        FontRenderContext frc = new FontRenderContext(null, true, true);
        for (int size : new int[]{12, 24}) {
            Font f = new Font("DejaVu Sans", Font.PLAIN, size);
            LineMetrics lm = f.getLineMetrics("Hello", frc);
            System.out.println("lm " + size + " asc=" + lm.getAscent() + " desc=" + lm.getDescent() + " lead=" + lm.getLeading() + " h=" + lm.getHeight()
                + " bounds=" + f.getStringBounds("Hello World", frc));
        }
        g.dispose();
        for (boolean aa : new boolean[]{false, true}) {
            for (int size : new int[]{12, 20}) {
                BufferedImage b = new BufferedImage(120, 30, BufferedImage.TYPE_INT_ARGB);
                Graphics2D gg = b.createGraphics();
                gg.setColor(Color.WHITE);
                if (aa) gg.setRenderingHint(RenderingHints.KEY_TEXT_ANTIALIASING, RenderingHints.VALUE_TEXT_ANTIALIAS_ON);
                gg.setFont(new Font("DejaVu Sans", Font.PLAIN, size));
                gg.drawString("Hello, AWT!", 3, 22);
                gg.dispose();
                System.out.println("text aa=" + aa + " size=" + size + " crc=" + crc(b));
                if (size == 12) for (int y = 8; y < 26; y++) { StringBuilder s = new StringBuilder(); for (int x = 0; x < 80; x++) { int p = b.getRGB(x, y) >>> 24; s.append(p == 0 ? '.' : p == 255 ? '#' : (char) ('0' + p * 10 / 256)); } System.out.println(s); }
            }
        }
    }
}
