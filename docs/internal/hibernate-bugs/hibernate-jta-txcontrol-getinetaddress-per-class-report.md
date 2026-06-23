# HIB-DEV-02 — Narayana `TxControl.<clinit>` NPE → **consolidated**

> **This doc was merged (2026-06-18) into the single Hibernate-JTA known-issue:**
> [hibernate-jta-narayana-xa-completion-and-socket-loopback.md](hibernate-jta-narayana-xa-completion-and-socket-loopback.md).
>
> HIB-DEV-02 (the `TxControl.<clinit>` `getHostAddress`-on-null crash) and the XA-completion /
> socket-loopback hang were the **same JTA cluster** described from two angles. The canonical doc now
> carries:
> - **Layer 0** — the entry crash (`ServerSocket.getInetAddress()` → null → `TxControl.<clinit>` NPE),
>   ✅ **FIXED** (`net_phase_e.rs`), including the ruled-out IPv4-vs-IPv6 `InetAddress.getLocalHost()`
>   red herring that originated in this report.
> - **Layer 1** — Narayana XA transaction completion never drives `XAResourceWrapper.commit()` → the H2
>   connection holds the `SIMPLEENTITY` lock → `@AfterEach truncate` `LOCK_TIMEOUT` hang. 🔴 OPEN.
> - **Layer 2** — synthetic `ServerSocket` accept/connect loopback never pairs. 🔴 OPEN.
>
> Affected classes, repros (`TxCtl.java`, `XaProbe.java`, `SSRepro.java`), and the Arjuna
> `BasicAction.End()` 1PC trace all live in the canonical doc. Nothing here is lost; this stub remains
> only so existing links resolve.
