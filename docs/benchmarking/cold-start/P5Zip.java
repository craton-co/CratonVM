import java.util.zip.*; import java.io.*;
public class P5Zip { public static void main(String[] a) throws Exception {
  CRC32 c = new CRC32(); c.update("hello".getBytes()); System.out.println("crc " + c.getValue());
  Adler32 ad = new Adler32(); ad.update("hello".getBytes()); System.out.println("adler " + ad.getValue());
  byte[] in = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbb".getBytes();
  Deflater d = new Deflater(); d.setInput(in); d.finish(); byte[] buf = new byte[200]; int n = d.deflate(buf); d.end();
  Inflater f = new Inflater(); f.setInput(buf, 0, n); byte[] out = new byte[200]; int m = f.inflate(out); f.end();
  System.out.println("deflate " + n + " inflate " + m + " eq " + new String(out,0,m).equals(new String(in)));
  ByteArrayOutputStream bo = new ByteArrayOutputStream(); try (GZIPOutputStream g = new GZIPOutputStream(bo)) { g.write(in); }
  try (GZIPInputStream gi = new GZIPInputStream(new ByteArrayInputStream(bo.toByteArray()))) { System.out.println("gzip " + new String(gi.readAllBytes()).length()); }
  ByteArrayOutputStream zo = new ByteArrayOutputStream(); try (ZipOutputStream z = new ZipOutputStream(zo)) { z.putNextEntry(new ZipEntry("x.txt")); z.write(in); z.closeEntry(); }
  try (ZipInputStream zi = new ZipInputStream(new ByteArrayInputStream(zo.toByteArray()))) { ZipEntry e = zi.getNextEntry(); System.out.println("zip " + e.getName() + " " + zi.readAllBytes().length); }
}}
