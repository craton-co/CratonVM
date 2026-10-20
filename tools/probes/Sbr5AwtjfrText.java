import java.awt.*;
import java.awt.image.*;
import java.util.zip.CRC32;

/** Text pixels per font, style, size and antialiasing mode; diff the output between HotSpot and CVM. */
public class Sbr5AwtjfrText {
    static String crc(BufferedImage b) {
        CRC32 c = new CRC32();
        for (int y = 0; y < b.getHeight(); y++)
            for (int x = 0; x < b.getWidth(); x++) {
                int p = b.getRGB(x, y);
                c.update(p >>> 24); c.update(p >> 16); c.update(p >> 8); c.update(p);
            }
        return Long.toHexString(c.getValue());
    }
    public static void main(String[] a) {
        Object[] aas = {RenderingHints.VALUE_TEXT_ANTIALIAS_OFF, RenderingHints.VALUE_TEXT_ANTIALIAS_ON,
            RenderingHints.VALUE_TEXT_ANTIALIAS_LCD_HRGB, RenderingHints.VALUE_TEXT_ANTIALIAS_LCD_VRGB};
        String[] aan = {"off", "gray", "lcd-hrgb", "lcd-vrgb"};
        for (String fam : new String[]{"Dialog", "Serif", "Monospaced", "DejaVu Sans"})
            for (int style : new int[]{Font.PLAIN, Font.BOLD, Font.ITALIC, Font.BOLD | Font.ITALIC})
                for (int size : new int[]{9, 12, 13, 18, 24})
                    for (int ai = 0; ai < 4; ai++)
                        for (boolean fm : new boolean[]{false, true}) {
                            BufferedImage b = new BufferedImage(260, 50, BufferedImage.TYPE_INT_RGB);
                            Graphics2D g = b.createGraphics();
                            g.setColor(Color.WHITE); g.fillRect(0, 0, 260, 50);
                            g.setColor(Color.BLACK);
                            g.setRenderingHint(RenderingHints.KEY_TEXT_ANTIALIASING, aas[ai]);
                            g.setRenderingHint(RenderingHints.KEY_FRACTIONALMETRICS,
                                fm ? RenderingHints.VALUE_FRACTIONALMETRICS_ON : RenderingHints.VALUE_FRACTIONALMETRICS_OFF);
                            g.setFont(new Font(fam, style, size));
                            g.drawString("Hamburgefonstiv, jQ 0123 AVWy", 3, 30);
                            g.dispose();
                            System.out.println(fam + " s" + style + " z" + size + " " + aan[ai] + " fm=" + fm + " " + crc(b));
                        }
    }
}
