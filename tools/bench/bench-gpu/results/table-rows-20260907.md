# Table rows re-run — 2026-09-07 00:07

RTX 2060 (sm_75). CratonVM: `target-gpu/release/cratonvm.exe`.
HotSpot: openjdk version "25.0.3" 2026-04-21 LTS.
TornadoVM 4.0.1-jdk25-ptx. N = 16777216 (2^24), best of 5, warm.

| Kernel | HotSpot C2 | TornadoVM | CratonVM GPU | vs HotSpot | vs TornadoVM | checksum |
|---|---|---|---|---|---|---|
| int div-chain | 2179 ms | 27 | **7 ms** | **311.3x** | **3.9x** | match |
| double div-chain | 1784 ms | 135 | **82 ms** | **21.8x** | **1.6x** | MISMATCH |
| 128 multiply-adds | 1298 ms | 26 | **7 ms** | **185.4x** | **3.7x** | match |
| dot-product reduction | 1168 ms | unimplemented | **2 ms** | **584.0x** | **n/a** | match |

Checksums (bit-exactness is the gate, not the speed):

| Kernel | HotSpot | CratonVM | TornadoVM |
|---|---|---|---|
| int div-chain | `246467335469` | `246467335469` | `246467335469` |
| double div-chain | `5.92801002867254E7` | `5.928010028745152E7` | `5.9583712290819384E7` |
| 128 multiply-adds | `-2050798668070082` | `-2050798668070082` | `-2050798668070082` |
| dot-product reduction | `-58730497593000` | `-58730497593000` | `-` |
