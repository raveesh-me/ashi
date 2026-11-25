# Concept 27: Client sessions; at-most-once processing

**Module:** M4 Consensus · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/reference/sessions.md`, all sections ("Lifecycle", "Eviction", "Retries", "Guarantees")
- `../tigerbeetle/docs/internals/vsr.md`, sections "Protocol: Client", "Protocol: Repair Client Replies"
- `../tigerbeetle/docs/ARCHITECTURE.md`, section "Hash-Chaining" ("Requests")
- `../tigerbeetle/docs/coding/reliable-transaction-submission.md`, sections "The App or Browser Should Generate the ID", "Handling Network Failures", "Handling Client Software Restarts"
- `../tigerbeetle/src/vsr/client.zig`: `parent`/`session`/`request_number` (74–83), `request_inflight` (96–98), `register` (277), `request` (330), `raw_request` (366–417), `on_eviction` (428–482), `on_reply` (529–641), `on_request_timeout` (663–680), `send_request_with_hedging` (736–750), `send_request_for_the_first_time` (752–786)
- `../tigerbeetle/src/vsr/client_sessions.zig`: VRR bugs comment (16–29), `Entry` (32–37), `encode_size` (80–97), `put` (228–263), `evictee` (265–299), `remove` (303)
- `../tigerbeetle/src/vsr/client_replies.zig`: header comment (1–20), `faulty` (88–94), `read_reply_sync` (141), `read_reply` (182), `write_reply` (328)
- `../tigerbeetle/src/vsr/replica.zig`: `ignore_request_message` (6266–6420), `ignore_request_message_duplicate` (6522–6663), `on_request_repeat_reply` (6665–6705), `ignore_request_message_preparing` (6734–6783), `primary_prepare_register` (7482), reply construction with `context` (5490–5511), `client_table_entry_create` (5716–5772), `client_table_entry_update` (5774–5814), `send_eviction_message_to_client` (8951), `send_reply_message_to_client` (8976–9013)
- `../tigerbeetle/src/vsr/message_header.zig`: `Request` (553–686), `Reply` (954–990), `Eviction` (1452–1515)
- `../tigerbeetle/src/vsr.zig`: `RegisterRequest` / `RegisterResult` (400–423)
- `../tigerbeetle/src/constants.zig` (75–85, 178, 255–270, 418–428), `../tigerbeetle/src/config.zig` (104, 228)

## 1. The problem

A client sends "create transfer T" and doesn't hear back. Three things could have happened:
1. The request never reached the cluster.
2. The cluster executed it, but the reply was lost.
3. The cluster is still working on it (slow network, view change in progress).

The client can't tell which. The only safe move is to **retry**. But a retry of case 2 must not execute T a second time. For a ledger that would be a double debit.

This is the classic exactly-once problem. A distributed system can't guarantee *exactly once delivery*. What it can offer is **at-most-once execution** of each request, plus retries until the client sees a reply. Together they give **exactly-once effect**, as long as the client keeps retrying.

The cluster must also do this with **bounded memory** (Concept 11). It can't remember every request forever.

## 2. The obvious solution, and why it fails

**"Remember every request ID ever processed."** Memory and disk grow without bound, and a lookup on every request costs a hash probe against an ever-growing set.

**"VRR's client table: remember the latest request number per client."** This is the textbook approach from Viewstamped Replication Revisited (VRR, Concept 21). TigerBeetle found two bugs in it (`client_sessions.zig:16–24`):

> 1. a correctness bug, where successive client crashes may cause request numbers to collide for different request payloads, resulting in requests receiving the wrong reply, and
> 2. a liveness bug, where if the client table is updated for request and prepare messages with the client's latest request number, then the client may be locked out from the cluster if the request is ever reordered through a view change.

Bug 1, concretely:
- A client process with ID 7 sends request #5 = "debit 100" and crashes.
- It restarts and reuses ID 7. Its counter starts again and soon reaches request #5 = "debit 999".
- The cluster sees "client 7, request 5" as a duplicate and replays the *old* reply. The client is told "debit 999 succeeded"; it never ran.

**"Evict old clients when the table fills up."** If eviction depends on anything replica-local (hash-map iteration order, wall time, arrival order), replicas evict *different* clients. Their client tables diverge, and so does their behaviour.

## 3. What TigerBeetle does

TigerBeetle's fix (`client_sessions.zig:25–29`):

> 1. register client sessions explicitly through the state machine to ensure that session numbers always increase, and
> 2. make a more careful distinction between uncommitted and committed request numbers, considering that uncommitted requests may not survive a view change.

### 3.1 Registration: sessions are committed through consensus

On start, a client generates a random, ephemeral `u128` client ID. The `Request.client` doc: "Each client process generates a unique, random and ephemeral client ID at initialization" (`message_header.zig:576–580`). A restarted process gets a **new** ID. It never resumes an old session (`sessions.md`, "Lifecycle").

Its first request is `operation = register` with `request = 0`, `parent = 0`, `session = 0` (validated in `Request.invalid_header`, `message_header.zig:628–641`). The register request is prepared and committed like any other op. The primary fills in the cluster's `batch_size_limit` (`primary_prepare_register`, `replica.zig:7482`).

When it commits:

```zig
// replica.zig:5723 (client_table_entry_create)
const session = reply.header.commit; // The commit number becomes the session number.
```

The client does the same on its side (`client.zig:629`). Why the commit number? It is **unique and strictly increasing across the whole cluster**, and every replica agrees on it. A session number can never repeat, even if a client ID is accidentally reused. That kills bug 1.

```
client                       primary                         all replicas
  │ request{op=register,       │                                │
  │   client=C, request=0}────►│ prepare op=1234 ──────────────►│ commit op 1234:
  │                            │                                │  client_sessions.put(
  │◄── reply{commit=1234,      │                                │    session=1234, C, reply)
  │     body=RegisterResult}   │
  │ session := 1234
  │ batch_size_limit := RegisterResult.batch_size_limit
```

### 3.2 The client-sessions table

`ClientSessions` (`client_sessions.zig`) has exactly `clients_max` slots:

```zig
pub const Entry = struct {
    /// The client's session number as committed to the cluster by a register request.
    session: u64,
    /// The header of the reply corresponding to the client's latest committed request.
    header: vsr.Header.Reply,
};
```

- `clients_max = 64` in the production config (`config.zig:228`; `constants.zig:81`). The docs agree: "`config.clients_max`, 64 by default" (`sessions.md`).
- It is serialized into a grid trailer at each checkpoint: 64 reply headers, then 64 session numbers. 64 × 256 B + 64 × 8 B = 16,896 bytes, asserted to fit in one block (`encode_size`, `client_sessions.zig:80–97`). Its checksum is part of `CheckpointState` (Concept 25).
- The table is keyed by client ID through `entries_by_client`, a hash map pre-sized to `clients_max` at init. Free entries are zeroed "both in `entries` and on-disk", so the on-disk bytes are deterministic.

Only **committed** state lives here. The entry is updated in `client_table_entry_update` during commit, never at prepare time. That is the fix for bug 2: a request reordered or dropped in a view change never advanced the client's entry, so the client isn't locked out.

### 3.3 Eviction: deterministic and least-recently-committed

When a new `register` commits and the table is full, one session is evicted (`client_table_entry_create`, `replica.zig:5739–5756`). The victim is the entry whose **latest reply has the smallest commit number** (`evictee`, `client_sessions.zig:265–299`):

```zig
/// For correctness, it's critical that all replicas evict deterministically:
/// We cannot depend on `HashMap.capacity()` since `HashMap.ensureTotalCapacity()` may
/// change across versions of the Zig std lib. We therefore rely on
/// `constants.clients_max`, which must be the same across all replicas ...
/// ... we do require that all entries have different commit numbers and are iterated.
/// This ensures that we will always pick the entry with the oldest commit number.
```

Commit numbers are unique, so there are never ties. The answer doesn't depend on iteration order.

Then the evicted client must find out. A later request from it reaches the primary, which has no entry for it, and gets an **eviction** message:

```zig
// Header.Eviction.Reason (message_header.zig:1499–1508)
no_session, client_release_too_low, client_release_too_high,
invalid_request_operation, invalid_request_body, invalid_request_body_size,
session_too_low, session_release_mismatch
```

The client then stops for good. It calls the eviction callback, or panics with "session evicted" if none is set (`client.zig:428–482`). From `sessions.md`: "After a session is evicted by the cluster, no future requests from that session will ever execute."

One subtlety: a request that was *already being prepared* when its session was evicted still commits. The replica sends the reply anyway, and "the next request will receive an eviction message" (`replica.zig:5811–5813`).

> ⚠️ **Docs vs code (wording).** `vsr.md` "Protocol: Client" says the cluster "evicts the oldest". The code evicts the session whose most recent *committed request* is oldest, not the oldest *registration*. `sessions.md` gets it right: "the session that committed a request the longest time ago."

### 3.4 Request numbers and one request in flight

Each request carries a `u32` request number. `register` is 0; each later request increments it (`raw_request`, `client.zig:393–394`). The client enforces **one in-flight request**: `assert(self.request_inflight == null)` (`client.zig:338, 372`). Further application requests queue inside the client library.

Why only one? `sessions.md`: "This simplifies consistency and allows the cluster to statically guarantee capacity in its incoming message queue." The primary's request queue is sized from it (`constants.zig:257–260`):

```
pipeline_request_queue_max = (clients_max + 1) - pipeline_prepare_queue_max
                           = (64 + 1) - 8 = 57
```

The `+1` is for the primary's own `pulse`/`upgrade` requests (client ID 0). With at most one request per client, prepares plus queued requests never exceed 65. No unbounded queue is possible.

On the primary, `ignore_request_message_duplicate` (`replica.zig:6522–6663`) compares the request with the client's committed entry:

| Incoming vs `entry.header.request` | Action |
|---|---|
| older session number | eviction `session_too_low` |
| request < entry.request | ignore (stale retry) |
| request == entry.request, same checksum | **resend cached reply** (`on_request_repeat_reply`) |
| request == entry.request, different checksum | ignore: "request collision (client bug)" |
| request == entry.request + 1, `parent == entry.context` | **new request**, go on to prepare |
| request == entry.request + 1, wrong parent | ignore: "client may have only one request inflight" |
| further ahead | ignore (client or network bug, or we're not the real primary) |
| no entry, not `register` | eviction `no_session` |

`ignore_request_message_preparing` (`replica.zig:6734–6783`) then catches a retry of a request that is **already in the pipeline** but not yet committed: "ignoring (already preparing)". Between the two checks, a request executes at most once, whether its retry arrives before or after commit.

### 3.5 Reply caching: the `client_replies` zone

To answer a duplicate, the replica must still have the reply:
- **Headers** live in the client-sessions table (above).
- **Bodies** live in a dedicated zone of the data file. One slot per client, each `message_size_max` long: `client_replies_size = clients_max * message_size_max` = 64 × 1 MiB = 64 MiB (`constants.zig:178`).
- Only replies with a non-empty body are written. Empty ones are rebuilt from the header (`client_replies.zig:7–8`; `replica.zig:6670–6676`).
- Writes are async and may be coalesced: "we only care about the last reply to each client session" (`client_replies.zig:11–12`). They are guaranteed durable at checkpoint.
- A corrupt reply is repaired from a peer via `get_reply` (Concept 25).

So the memory bound for at-most-once is fixed: 64 headers in RAM, 64 MiB on disk, forever.

### 3.6 The request/reply hash chain

Each session is a hash chain (`client.zig:74–77`):

```
 register ──► reply₀ ──► request₁ ──► reply₁ ──► request₂ ──► …
          req_cksum   parent=ctx₀  req_cksum   parent=ctx₁
```

- `request.parent` = the previous reply's `context` (`client.zig:598, 766`).
- `reply.request_checksum` = the checksum of the request it answers. The client asserts it: `assert(reply.header.request_checksum == self.parent)` (`client.zig:590`).
- The primary accepts request N+1 only if `message.header.parent == entry.header.context`: "The client has proved that they received our last reply" (`replica.zig:6598–6601`).

**What is `context`?** It is "almost exactly the same as entire header's checksum, except that it is computed with a fixed view and remains stable if reply is retransmitted in a newer view" (`message_header.zig:978–982`). The replica computes it once, before setting the real checksum: `reply.header.context = reply.header.calculate_checksum();` (`replica.zig:5509`). When a reply is resent with a bumped view number (`send_reply_message_to_client`, `replica.zig:8976–9013`), its `checksum` changes but its `context` does not. The client's chain stays valid.

Why bother, when request numbers already order things? Request and commit numbers "have low entropy and may otherwise collide in the event of any correctness bugs" (`message_header.zig:981–982`). A 128-bit checksum chain turns a silent mix-up into a detected one. `ARCHITECTURE.md`: they "provide a strong proof that the proper ordering of requests is observed."

> ⚠️ **Docs vs code (precision).** `ARCHITECTURE.md` says "Each client request includes a checksum of the reply to the previous request." Strictly, `parent` is the reply's `context`: its header checksum computed with `context = 0` in the original view, not the reply's final `checksum` field. The intent is the same; the code is precise.

### 3.7 Retries never give up

The client resends on `request_timeout` with exponential backoff and jitter (`client.zig:663–680`). Each send goes to the primary *and* one random backup ("hedging", `client.zig:736–750`), so a broken client→primary link doesn't stall it. Per `sessions.md`: "the TigerBeetle client will never time out", has "no retry limits", and "does not surface network errors". An error would imply the request didn't execute, "when that is not known".

### 3.8 How this composes with end-to-end idempotency (Concept 4)

Sessions give **at-most-once per request, within one session**. They stop at the client process boundary:
- If the application process restarts, it gets a new client ID and session. The old in-flight request "may occur at any point in the future, or never" (`sessions.md`, "Guarantees").
- If the client is evicted, unanswered requests are in limbo too.

The second layer is the **object ID** (Concept 4). Every `Transfer` and `Account` carries a client-generated `u128` `id`. Creating an object with an existing `id` returns `exists` instead of creating a duplicate (`reliable-transaction-submission.md`; `exists = 46` for transfers, `tigerbeetle.zig:251`).

```
 layer               scope                       guards against
 ─────────────────── ─────────────────────────── ────────────────────────────────
 request number +    one session (one client     network retries, duplicate packets,
 session + reply     process lifetime)           lost replies → resend cached reply
 cache + hash chain
 transfer.id         forever, across sessions,   app/process restarts, gateway retries,
 (idempotency key)   gateways, browsers          evicted sessions
```

Together: the session layer makes retries cheap and *transparent* (the same reply comes back). The ID layer makes them *safe across restarts*. The result is end-to-end exactly-once effect. Each layer covers a gap in the other.

## 4. What it means for ashi

**Rust features that help:**
- Newtypes: `struct ClientId(u128)`, `struct SessionNumber(u64)`, `struct RequestNumber(u32)`. Mixing up a session number and a request number becomes a compile error.
- `#[repr(C)]` 256-byte `Request`/`Reply` headers with static size asserts, as in Concepts 24 and 28.
- A fixed `[Entry; CLIENTS_MAX]` plus a bitset for "present" slots, with the client-ID → index map sized once at startup.

**Patterns to copy:**
- Session number = commit op of `register`. Free, unique, monotonic.
- Update the session table **only at commit**. Never at prepare.
- Evict by minimum last-commit number, so the choice is deterministic without any tie-break.
- One in-flight request per client, so every queue has a static bound.
- Cache the last reply per session. Keep `context` stable across view-bumped retransmits.

**Pitfalls specific to Rust:**
- `std::collections::HashMap` with the default `RandomState` gives **different iteration order on every replica**. Never iterate it to choose an evictee. TigerBeetle iterates but picks the minimum of unique keys, which is order-independent. Do the same, or iterate the fixed array.
- `HashMap::with_capacity` may allocate more than asked, and growth policy can change between Rust versions. TigerBeetle avoids depending on `HashMap.capacity()` for the same reason. Base the "table full" check on your own `CLIENTS_MAX` counter.
- A per-client `tokio::sync::Mutex` or channel is the wrong model here. Everything runs on the single commit thread (Concept 6), so plain owned data works.
- Generate client IDs from a CSPRNG (`getrandom`), not a seeded PRNG. Two processes with the same seed would collide.

**Crates:** none needed. `getrandom` for client IDs if you accept one tiny dependency. Otherwise read `/dev/urandom`.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt.** The design is small, bounded, and fixes known bugs in VRR. The one knob to revisit is `clients_max`: ashi's gateway architecture (Concept 4) decides whether 64 concurrent sessions is enough.

## 5. Drill questions

### Q1. Recall

What becomes a client's session number? What is `clients_max` by default, and how large is the `client_replies` zone as a result?

> _Your answer:_

### Q2. Mechanism

Explain the VRR "client crashes and reuses its ID" correctness bug, and exactly which TigerBeetle mechanism prevents it.

> _Your answer:_

### Q3. Back-of-envelope

With the default config, how many requests can be queued at the primary (`pipeline_request_queue_max`)? If ashi raised `clients_max` to 1,024 with 1 MiB messages, how big would the reply cache zone be, and the client-sessions trailer (headers + session numbers)?

> _Your answer:_

### Q4. What breaks if…

…the primary updated a client's session entry (its latest request number) when it *prepared* the request, instead of when it committed it? Describe a sequence with a view change.

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle stops at-most-once at the session boundary and relies on `transfer.id` for the rest. Should ashi instead offer *durable* sessions that survive client restarts (e.g. the client persists its session number and resumes)? What would that cost in cluster memory, eviction policy and failure modes?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** The **commit number (op)** of its `register` request: `session = reply.header.commit`. `clients_max = 64`. `client_replies_size = clients_max × message_size_max = 64 × 1 MiB = 64 MiB`.

**Q2.** With VRR's client table keyed by (client ID, request number), a client that crashes and restarts with the same ID restarts its request counter. Its new request #5 collides with the old request #5, and the cluster replays the *old* reply for a *different* payload. TigerBeetle prevents this in three ways. (a) Client IDs are random 128-bit values generated fresh per process, so reuse is improbable. (b) Sessions are registered through consensus, and the session number is the register's commit op, which is unique and monotonic. A request with an older session gets `session_too_low`. (c) A duplicate is only answered from cache if `message.header.checksum == entry.header.request_checksum`. A same-number request with a different payload is a "request collision" and is ignored, never answered with the wrong reply.

**Q3.** `(64 + 1) − 8 = 57`. With `clients_max = 1,024`: the reply zone is 1,024 × 1 MiB = **1 GiB**. The sessions trailer is 1,024 × 256 B + 1,024 × 8 B = 262,144 + 8,192 = **270,336 B ≈ 264 KiB**. That still fits in one 512 KiB block, which `encode_size` asserts (`size_max <= block_size - @sizeOf(vsr.Header)`). At about 2,000 clients it would not, and the trailer encoding would need to span blocks. Also `pipeline_request_queue_max` would become 1,017, and the message pool grows with it (Concept 11).

**Q4.** Sequence: client C sends request #5. Primary P1 prepares it as op 100 and sets C's entry to request 5. P1 crashes before op 100 reaches a replication quorum. The view change truncates op 100 (it was uncommitted, Concept 24). The new primary P2 (or P1 later, with its stale in-memory table) believes C's last request is 5. C retries #5. It is treated as a duplicate ("request == entry.request"), but there is no committed reply to resend. Or the checksums differ in other ways and it's ignored. Meanwhile #6 is rejected because #5 never committed, so C is **locked out**. That's VRR bug 2. Updating at commit means only committed requests advance the entry.

**Q5.** Open-ended. Durable sessions would need (1) more session slots, or a longer-lived eviction policy, so a restarted client still finds its entry; (2) the client to persist its session number, request number and `parent` before each send; (3) a rule for a client that resumes after being evicted, which would still get an eviction error. Benefit: transparent retries across restarts without the app tracking IDs. Cost: unbounded or much larger session tables, and harder eviction semantics. It also doesn't remove the need for `transfer.id`, because the app→gateway hop (Concept 4) still needs idempotency. TigerBeetle's choice keeps the cluster bounded and pushes durability to IDs the app already has.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
