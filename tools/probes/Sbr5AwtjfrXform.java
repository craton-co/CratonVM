import java.awt.*;
import java.awt.font.*;
import java.awt.geom.*;
import java.awt.image.*;
import java.io.File;
import java.util.zip.CRC32;

/** Text with transformed and synthetic-style fonts; diff the output between HotSpot and CVM. */
public class Sbr5AwtjfrXform {
    static String crc(BufferedImage b) {
        CRC32 c = new CRC32();
        for (int y = 0; y < b.getHeight(); y++)
            for (int x = 0; x < b.getWidth(); x++) {
                int p = b.getRGB(x, y);
                c.update(p >>> 24); c.update(p >> 16); c.update(p >> 8); c.update(p);
            }
        return Long.toHexString(c.getValue());
    }
    static void draw(String tag, Font f, AffineTransform gt, Object aa, boolean fm) {
        BufferedImage b = new BufferedImage(200, 120, BufferedImage.TYPE_INT_RGB);
        Graphics2D g = b.createGraphics();
        g.setColor(Color.WHITE); g.fillRect(0, 0, 200, 120);
        g.setColor(Color.BLACK);
        g.setRenderingHint(RenderingHints.KEY_TEXT_ANTIALIASING, aa);
        g.setRenderingHint(RenderingHints.KEY_FRACTIONALMETRICS,
            fm ? RenderingHints.VALUE_FRACTIONALMETRICS_ON : RenderingHints.VALUE_FRACTIONALMETRICS_OFF);
        g.translate(100, 60);
        if (gt != null) g.transform(gt);
        g.setFont(f);
        g.drawString("Hamburgefonstiv, jQ 0123", -90, 0);
        g.dispose();
        FontMetrics m = new BufferedImage(1, 1, BufferedImage.TYPE_INT_RGB).createGraphics().getFontMetrics(f);
        System.out.println(tag + " aa=" + (aa == RenderingHints.VALUE_TEXT_ANTIALIAS_OFF ? "off" : aa == RenderingHints.VALUE_TEXT_ANTIALIAS_ON ? "on" : "lcd") + " fm=" + fm + " " + crc(b)
            + " w=" + m.stringWidth("Hamburgefonstiv, jQ 0123") + " mx=" + m.getMaxAdvance());
    }
    public static void main(String[] a) throws Exception {
        Object[] aas = {RenderingHints.VALUE_TEXT_ANTIALIAS_OFF, RenderingHints.VALUE_TEXT_ANTIALIAS_ON, RenderingHints.VALUE_TEXT_ANTIALIAS_LCD_HRGB};
        AffineTransform[] gts = {
            AffineTransform.getRotateInstance(Math.toRadians(15)), AffineTransform.getRotateInstance(Math.toRadians(90)),
            AffineTransform.getRotateInstance(Math.toRadians(180)), AffineTransform.getScaleInstance(1.5, 1.5),
            AffineTransform.getScaleInstance(1.0, 2.0), AffineTransform.getShearInstance(0.3, 0), AffineTransform.getScaleInstance(-1, 1)};
        String[] gn = {"rot15", "rot90", "rot180", "scale1.5", "scale1x2", "shear", "mirror"};
        for (String fam : new String[]{"Dialog", "Serif", "DejaVu Sans Mono"})
            for (int size : new int[]{12, 18})
                for (int gi = 0; gi < gts.length; gi++)
                    for (Object aa : aas) for (boolean fm : new boolean[]{false, true})
                        draw(fam + " z" + size + " g" + gn[gi], new Font(fam, Font.PLAIN, size), gts[gi], aa, fm);
        // derived fonts carrying the transform
        for (int size : new int[]{12, 18})
            for (int gi = 0; gi < gts.length; gi++)
                for (Object aa : aas) for (boolean fm : new boolean[]{false, true})
                    draw("derive z" + size + " " + gn[gi], new Font("Dialog", Font.PLAIN, size).deriveFont(gts[gi]), null, aa, fm);
        // created fonts: styles that need synthesis
        Font[] bases = {Font.createFont(Font.TRUETYPE_FONT, new File("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf")),
                        Font.createFont(Font.TRUETYPE_FONT, new File("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"))};
        for (int bi = 0; bi < bases.length; bi++)
            for (int style : new int[]{Font.PLAIN, Font.BOLD, Font.ITALIC, Font.BOLD | Font.ITALIC})
                for (int size : new int[]{9, 12, 18, 24})
                    for (Object aa : aas) for (boolean fm : new boolean[]{false, true})
                        draw("created" + bi + " s" + style + " z" + size, bases[bi].deriveFont(style, size), null, aa, fm);
    }
}
