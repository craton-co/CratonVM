// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

import java.awt.Font;
import java.awt.font.FontRenderContext;
import java.awt.font.GlyphVector;
import java.awt.font.TextAttribute;
import java.awt.geom.AffineTransform;
import java.io.File;
import java.util.Arrays;
import java.util.Map;
public class CiFontShapeProbe {
  public static void main(String[] args) throws Exception {
    Font font = Font.createFont(Font.TRUETYPE_FONT, new File(args[0])).deriveFont(18f);
    FontRenderContext ctx = new FontRenderContext(new AffineTransform(), true, true);
    String[] texts = {"office", "a\u0301fi", "\u0633\u0644\u0627\u0645", "\u05e9\u05dc\u05d5\u05dd", "A\u200dB"};
    Font featured = font.deriveFont(Map.of(TextAttribute.KERNING, TextAttribute.KERNING_ON,
                                           TextAttribute.LIGATURES, TextAttribute.LIGATURES_ON));
    for (Font current : new Font[] {font, featured}) {
      for (int k = 0; k < texts.length; k++) {
        char[] chars = texts[k].toCharArray();
        GlyphVector g = current.layoutGlyphVector(ctx, chars, 0, chars.length, k == 2 || k == 3 ? Font.LAYOUT_RIGHT_TO_LEFT : Font.LAYOUT_LEFT_TO_RIGHT);
        int n = g.getNumGlyphs();
        System.out.println(k + " " + Arrays.toString(g.getGlyphCodes(0, n, null)) + " " + Arrays.toString(g.getGlyphCharIndices(0, n, null)) + " " + Arrays.toString(g.getGlyphPositions(0, n + 1, null)));
      }
    }
  }
}
