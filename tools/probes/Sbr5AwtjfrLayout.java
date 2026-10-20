import java.awt.*;
import java.awt.font.*;
import java.awt.geom.*;

/** Glyph layout (shaping) results per text and font; diff the output between HotSpot and CVM. */
public class Sbr5AwtjfrLayout {
    static final String[][] TEXTS = {
        {"latin", "Hello, World! AVATAR To Wa fi fl ffi ffl office"},
        {"kern", "AV AW AY TA To Tr Ty VA Va We Yo"},
        {"arabic", "\u0645\u0631\u062D\u0628\u0627 \u0628\u0627\u0644\u0639\u0627\u0644\u0645"},
        {"arabic-lig", "\u0644\u0627 \u0644\u0623 \u0644\u0625 \u0644\u0622 \u0627\u0644\u0644\u0647"},
        {"arabic-marks", "\u0643\u064E\u062A\u064E\u0628\u064E \u0628\u0650\u0633\u0652\u0645\u0650"},
        {"hebrew", "\u05E9\u05DC\u05D5\u05DD \u05E2\u05D5\u05DC\u05DD"},
        {"hebrew-marks", "\u05D1\u05B0\u05BC\u05E8\u05B5\u05D0\u05E9\u05B4\u05C1\u05D9\u05EA"},
        {"mixed-bidi", "abc \u05D0\u05D1\u05D2 def \u0627\u0628\u062A 123"},
        {"devanagari", "\u0928\u092E\u0938\u094D\u0924\u0947 \u0926\u0941\u0928\u093F\u092F\u093E \u0915\u094D\u0937\u0924\u094D\u0930"},
        {"thai", "\u0E2A\u0E27\u0E31\u0E2A\u0E14\u0E35\u0E0A\u0E32\u0E27\u0E42\u0E25\u0E01"},
        {"combining", "e\u0301 a\u0308 o\u0302 n\u0303 A\u030A \u0061\u0323\u0302"},
        {"greek-cyr", "\u0393\u03B5\u03B9\u03AC \u03C3\u03BF\u03C5 \u041F\u0440\u0438\u0432\u0435\u0442 \u043C\u0438\u0440"},
        {"emoji", "\uD83D\uDE00 \uD83D\uDC4D\uD83C\uDFFD \uD83D\uDC68\u200D\uD83D\uDC69\u200D\uD83D\uDC67 \u2764\uFE0F \uD83C\uDDFA\uD83C\uDDF8"},
        {"zwj-zwnj", "\u0646\u200C\u0647 \u0646\u200D\u0647 a\u200Db"},
        {"digits-punct", "1,234.56 (a+b)=c; 100% \u2014 \u201Cquoted\u201D \u2026"},
    };
    public static void main(String[] a) {
        FontRenderContext[] frcs = {new FontRenderContext(null, true, true), new FontRenderContext(null, false, false)};
        String[] fams = {"DejaVu Sans", "DejaVu Serif", "Dialog", "DejaVu Sans Mono"};
        for (String[] t : TEXTS)
            for (String fam : fams)
                for (int fi = 0; fi < frcs.length; fi++)
                    for (int flags : new int[]{Font.LAYOUT_LEFT_TO_RIGHT, Font.LAYOUT_RIGHT_TO_LEFT}) {
                        Font f = new Font(fam, Font.PLAIN, 24);
                        char[] cs = t[1].toCharArray();
                        StringBuilder sb = new StringBuilder(t[0] + " " + fam + " frc" + fi + " fl" + flags + ":");
                        try {
                            GlyphVector gv = f.layoutGlyphVector(frcs[fi], cs, 0, cs.length, flags);
                            int n = gv.getNumGlyphs();
                            sb.append(" n=").append(n);
                            for (int i = 0; i < n; i++) {
                                Point2D p = gv.getGlyphPosition(i);
                                sb.append(' ').append(gv.getGlyphCode(i)).append('@').append(String.format("%.2f,%.2f", p.getX(), p.getY()))
                                  .append('c').append(gv.getGlyphCharIndex(i));
                            }
                            Point2D end = gv.getGlyphPosition(n);
                            sb.append(" end=").append(String.format("%.2f", end.getX()));
                            Rectangle2D lb = gv.getLogicalBounds();
                            sb.append(String.format(" lb=%.2f,%.2f,%.2f,%.2f", lb.getX(), lb.getY(), lb.getWidth(), lb.getHeight()));
                        } catch (Throwable e) {
                            sb.append(" EXC ").append(e);
                        }
                        System.out.println(sb);
                    }
        // TextLayout carets and bounds
        for (String[] t : TEXTS) {
            try {
                TextLayout tl = new TextLayout(t[1], new Font("DejaVu Sans", Font.PLAIN, 24), frcs[0]);
                StringBuilder sb = new StringBuilder("TL " + t[0] + String.format(" adv=%.2f asc=%.2f dsc=%.2f", tl.getAdvance(), tl.getAscent(), tl.getDescent()));
                sb.append(" rtl=").append(tl.isLeftToRight());
                for (int i = 0; i <= t[1].length(); i += 2) { float[] ci = tl.getCaretInfo(TextHitInfo.afterOffset(i)); sb.append(String.format(" %.2f", ci[0])); }
                System.out.println(sb);
            } catch (Throwable e) { System.out.println("TL " + t[0] + " EXC " + e); }
        }
    }
}
