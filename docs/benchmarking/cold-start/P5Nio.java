import java.nio.file.*; import java.nio.*; import java.nio.charset.*; import java.io.*; import java.util.*;
public class P5Nio { public static void main(String[] a) throws Exception {
  Path p = Files.createTempFile("p5", ".txt"); Files.write(p, List.of("one", "two", "три"), StandardCharsets.UTF_8);
  System.out.println(Files.readAllLines(p) + " " + Files.size(p) + " " + Files.exists(p));
  try (BufferedReader r = new BufferedReader(new FileReader(p.toFile()))) { System.out.println(r.readLine()); }
  try (RandomAccessFile raf = new RandomAccessFile(p.toFile(), "r")) { raf.seek(4); System.out.println((char) raf.read() + " " + raf.length()); }
  ByteBuffer b = ByteBuffer.allocateDirect(16); b.putInt(42).putLong(7L).flip(); System.out.println(b.getInt() + " " + b.getLong() + " " + b.order());
  System.out.println(StandardCharsets.UTF_8.encode("héllo").remaining() + " " + Charset.defaultCharset());
  try (DataOutputStream d = new DataOutputStream(new FileOutputStream(p.toFile()))) { d.writeInt(5); d.writeUTF("x"); }
  try (DataInputStream d = new DataInputStream(new FileInputStream(p.toFile()))) { System.out.println(d.readInt() + d.readUTF()); }
  Files.delete(p); System.out.println(Files.exists(p) + " " + Paths.get("/a/b/../c").normalize());
}}
