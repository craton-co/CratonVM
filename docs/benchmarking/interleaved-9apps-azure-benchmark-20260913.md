# 9 Applications Interleaved Benchmark: CratonVM vs HotSpot

- **Date:** Sun Sep 13 14:20:33 UTC 2026
- **Host:** Azure EPYC Linux Host (`x86_64`, 8 vCPUs)
- **CratonVM Binary:** `/data/wt-bench-9apps/bin/cratonvm-bench-20260913125913` (dev branch @ `2a8aa3888`)
- **HotSpot Baseline:** `/data/toolchain/jdk-25` (Temurin 25.0.4+7-LTS)
- **Mode:** Interleaved runs (CratonVM / HotSpot alternating)

| Application / Workload | Suite / Probe Description | HotSpot (ms) | CratonVM (ms) | Ratio (CV / HS) | Verdict |
| :--- | :--- | :---: | :---: | :---: | :---: |
| **Spring Framework** | Core AOT Generation Suite (AccessControlTests) | 598 ms | 861 ms | 1.44x | PASS (median 1.44x) |
| **Spring Boot** | Smoke batch ApplicationEnvironmentTests | 1579 ms | 1520 ms | 0.96x | PASS (median 0.96x) |
| **Tomcat** | Jakarta EL Core Tests (TestArrayELResolver) | 18 ms | 18 ms | 1.00x | PASS (median 1.00x) |
| **Netty** | Unix Native Inet Address & Channel Suite | 552 ms | 653 ms | 1.18x | PASS (median 1.18x) |
| **Hibernate ORM** | Bytecode enhancement & entity tests | 464 ms | 464 ms | 1.00x | PASS (median 1.00x) |
| **Hibernate Reactive / PostgreSQL** | Reactive CompletionStages & SCRAM against Docker Postgres | 1144 ms | 1146 ms | 1.00x | PASS (median 1.00x) |
| **H2 Database** | Database Alter & DDL Table Suite (TestAlter) | 297 ms | 772 ms | 2.60x | PASS (median 2.60x) |
| **Apache Commons Math** | Core JdkMath / FastMath Numerical Kernel | 80 ms | 203 ms | 2.54x | PASS (median 2.54x) |
| **Bouncy Castle Java** | AESFast / Symmetric Cipher Validation | 60 ms | 629 ms | 10.48x | PASS (median 10.48x) |
