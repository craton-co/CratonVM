import java.math.*; import java.time.*; import java.text.*; import java.util.*; import java.util.concurrent.*; import java.io.*; import java.net.*;
public class P5Misc { public static void main(String[] a) throws Exception {
  System.out.println(new BigDecimal("1.10").add(new BigDecimal("2.205")) + " " + BigInteger.TWO.pow(100) + " " + new BigDecimal("10").divide(new BigDecimal("3"), MathContext.DECIMAL64));
  System.out.println(LocalDate.of(2024, 2, 28).plusDays(2) + " " + Duration.ofMinutes(135) + " " + LocalDateTime.of(2020,1,1,10,0).getDayOfWeek() + " " + ZoneOffset.UTC);
  System.out.println(String.format("%05d|%.3f|%s|%x", 42, Math.PI, "s", 255) + " " + new DecimalFormat("#,##0.00").format(1234567.891) + " " + MessageFormat.format("{0} has {1}", "x", 3));
  SimpleDateFormat sdf = new SimpleDateFormat("yyyy-MM-dd"); sdf.setTimeZone(TimeZone.getTimeZone("UTC")); System.out.println(sdf.format(new Date(0)));
  System.out.println(CompletableFuture.supplyAsync(() -> 21).thenApply(x -> x * 2).get());
  ExecutorService ex = Executors.newFixedThreadPool(2); Future<Integer> f = ex.submit(() -> 7); System.out.println(f.get()); ex.shutdown();
  System.out.println(ForkJoinPool.commonPool().submit(() -> 5).get() + " " + new ConcurrentSkipListMap<>(Map.of(2,"b",1,"a")));
  ByteArrayOutputStream bo = new ByteArrayOutputStream(); try (ObjectOutputStream o = new ObjectOutputStream(bo)) { o.writeObject(new ArrayList<>(List.of("s", 1))); }
  try (ObjectInputStream i = new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray()))) { System.out.println(i.readObject()); }
  System.out.println(new URI("http://h:8080/p?q=1").getPort() + " " + InetAddress.getLoopbackAddress().getHostAddress() + " " + URLEncoder.encode("a b&c", "UTF-8"));
  System.out.println(UUID.nameUUIDFromBytes("x".getBytes()) + " " + Base64.getEncoder().encodeToString("hi".getBytes()) + " " + java.security.MessageDigest.getInstance("SHA-256").digest("a".getBytes()).length);
  System.out.println(Object.class.getResource("Object.class") != null);
  System.out.println(ClassLoader.getSystemResource("java/lang/Object.class") != null);
  System.out.println(Class.forName("java.lang.String").getMethod("length").invoke("abc") + " " + new Vector<>(List.of(1)) + new Stack<>() + new PriorityQueue<>(List.of(3,1,2)).peek());
}}
