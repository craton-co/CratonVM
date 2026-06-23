# Bug F — CratonVM broker-integration test gaps (OPEN)

| | |
|---|---|
| **Kind** | FAIL / TIMEOUT on real-broker integration tests |
| **CratonVM** | FAIL/TIMEOUT · **HotSpot (broker up)** OK |
| **Status** | OPEN — ≥9 confirmed (floor; full idle re-measure blocked by concurrent-build contention) |

## How it surfaced

The original sweep ran with **no Kafka broker**, so a set of integration tests failed on
*both* HotSpot and CratonVM and were excluded as env failures. Standing up a real broker
(docker `apache/kafka:latest`, `localhost:9092`) and re-running showed HotSpot **passes** them
— so they are genuine tests, and CratonVM's failures on them are real CratonVM-only gaps that
the no-broker run had masked. (`EndToEndClusterIdTest`: fail-on-both → **HS 4/4 OK** with
broker.)

## Confirmed gaps (HotSpot OK with broker, CratonVM fails)

| class | CratonVM |
|---|---|
| `controller.QuorumControllerTest` | TIMEOUT |
| `security.authorizer.AuthorizerTest` | FAIL |
| `tiered.storage.integration.TransactionsWithMaxInFlightOneTest` | FAIL |
| `tools.AclCommandTest` | TIMEOUT |
| `tools.MetadataQuorumCommandTest` | FAIL |
| `tools.consumer.ConsoleConsumerTest` | TIMEOUT |
| `tools.consumer.group.DeleteConsumerGroupsTest` | FAIL |
| `tools.consumer.group.SaslClientsWithInvalidCredentialsTest` | TIMEOUT |
| `tools.consumer.group.ShareGroupCommandTest` | TIMEOUT |

These are heavy `tools`/`server`/`controller` cluster tests (embedded KRaft + admin/consumer
client flows). The TIMEOUT/FAIL split was measured under build contention and is not final.

## Notes / next steps

- `≥9` is a **floor**: the broker re-run's HS side also showed ~31 ABEND + ~19 TIMEOUT that are
  contention false-positives; several of those are likely additional broker-env tests that
  would pass HS idle → more CratonVM gaps hidden underneath.
- Likely root causes overlap Bug E (Mockito/ByteBuddy), the async/executor path, and embedded
  cluster bring-up. Triage each on an **idle** box with the broker up and the enhanced KRun
  (full stack traces).
- Repro env: docker container `craton-kafka` (`apache/kafka:latest`), `localhost:9092`.
