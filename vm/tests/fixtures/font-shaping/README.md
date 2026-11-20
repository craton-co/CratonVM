`shaping.ttf` is a subset of DejaVu Sans 2.37, renamed to Craton CI Shaping
Fixture. It retains ASCII, U+0301, the Hebrew and Arabic letters in the probe,
and U+200D, with all layout features. The font was subset with fontTools;
`LICENSE.txt` contains its license. The probe compares glyph IDs, UTF-16
clusters, and positions with the same font on HotSpot and CratonVM.
