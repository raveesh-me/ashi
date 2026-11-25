# Concept 4: Idempotency keys that follow a transfer end to end; gateways that batch requests

**Module:** M0 The problem · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, section "Systems Thinking"
- `../tigerbeetle/docs/coding/system-architecture.md`
- `../tigerbeetle/docs/coding/reliable-transaction-submission.md`
- `../tigerbeetle/docs/coding/data-modeling.md`, sections "`id`", "TigerBeetle Time-Based Identifiers" and "`user_data`"
- `../tigerbeetle/docs/coding/requests.md`, sections "Idempotency", "Batching Events", "Automatic Batching" and "Guarantees"
- `../tigerbeetle/docs/reference/requests/create_transfers.md`, sections "`exists`" and "`id_already_failed`"
- `../tigerbeetle/docs/reference/sessions.md`
- `../tigerbeetle/src/state_machine.zig` (`create_transfer`, `create_transfer_exists`, `transient_error`, `execute_create`)
- `../tigerbeetle/src/tigerbeetle.zig` (`CreateTransferStatus`, `transient`, `CreateTransferResult`)
- `../tigerbeetle/src/vsr/multi_batch.zig` (`event_max`, `trailer_total_size`)
- `../tigerbeetle/src/clients/rust/src/time_based_id.rs`
- `../tigerbeetle/src/state_machine_tests.zig`, test "linked accounts" and the `TestContext` comment

## 1. The problem

A payment travels through many hops: phone → load balancer → API service → database client → database primary → replicas, then back. Any hop can fail **ambiguously**:

```
app ──req──▶ gateway ──req──▶ TigerBeetle
                                  │ executes the transfer
app ◀─ ✗ ── gateway ◀── reply ────┘   reply lost / gateway crashes / app restarts
```

From the app's side, "no reply" means "unknown". The transfer may or may not have happened. Every layer that retries (browser, HTTP library, load balancer, gateway, database client) risks executing it **twice**. In a ledger, executing a transfer twice is a double charge.

A second problem is scale of *connections*. Millions of end users can't each hold a database session. TigerBeetle has a hard limit, `clients_max = 64` in the production config (`src/config.zig:228`), and each session may have at most one request in flight (`reference/sessions.md`).

## 2. The obvious solution, and why it fails

- **The server assigns ids (auto-increment).** A retry looks like a new request and gets a new id, so you get a duplicate transfer. The counter is also a central oracle and a bottleneck (`data-modeling.md`, "`id`").
- **Deduplicate in the API layer.** Store `Idempotency-Key → response` in Redis or Postgres at the gateway. Now the gateway is stateful. The key store and the ledger are two systems with no shared commit: crash after writing the key but before the ledger write (or the other way round) and they disagree. A retry routed to a different gateway or region may miss the key.
- **Rely on the database session.** TigerBeetle's client sessions do deduplicate *requests* (Concept 27), but only within one session. "When a client restarts… it starts a new session, with a new (random) client id". And "Requests retried by a different client (same request body, different session) may receive different replies" (`sessions.md`, `requests.md`).
- **One database connection per end user.** That hits `clients_max` immediately, and sessions get evicted.

Every one of these puts the key in the wrong place: too late in the path, or not durable enough.

## 3. What TigerBeetle does

`ARCHITECTURE.md`, "Systems Thinking", states the whole pattern:

> - Each transfer carries an end-to-end idempotency key: a unique 128-bit ID generated and persisted by the end application (e.g. a mobile phone or a website).
> - Applications do not submit transfers to TigerBeetle directly, going instead through an API gateway.
> - The gateway aggregates individual transfers from separate applications into large batches.
> - Gateways are stateless and horizontally scalable. All state is managed by TigerBeetle.
> - End-to-end idempotency keys guarantee that each transfer is processed at most once, even if, due to retry and load-balancing logic, it gets routed through several gateways.

### 3.1 The 128-bit id, generated at the edge

`Transfer.id` and `Account.id` are `u128`. The value must not be `0` or `2^128 - 1`, and it is unique per cluster (accounts and transfers are separate namespaces). `reliable-transaction-submission.md` gives the protocol:

```
1. user initiates a transfer
2. app generates the id
3. app PERSISTS the id locally          ← before anything leaves the device
4. app → API service
5. API service puts it in a request
6. TigerBeetle creates it once and only once
7-8. replies flow back
on any failure or restart: resubmit with the SAME id
```

The recommended format is the **TigerBeetle time-based identifier (TBID)**:

```
 127                  80 79                                   0
┌──────────────────────┬──────────────────────────────────────┐
│ ms since Unix epoch  │ random                               │
│ 48 bits              │ 80 bits (incremented within same ms) │
└──────────────────────┴──────────────────────────────────────┘
id = (timestamp << 80) | random
```

The official Rust client implements it in `src/clients/rust/src/time_based_id.rs:30`. If the clock hasn't advanced (or went backwards), it just adds 1 to the previous id:

```rust
if time_ms_new > self.previous {
    self.previous = time_ms_new | Self::entropy();
} else {
    // Same/backward time: increment, carrying into the timestamp on entropy overflow.
    self.previous = self.previous.checked_add(1).expect("time overflow");
}
```

Why time-ordered rather than random: ids are looked up in an LSM tree (Concept 18). Mostly-increasing keys land in recent, cached tables. `data-modeling.md` says random ids have "_significantly_ lower throughput than strictly-increasing ULIDs".

### 3.2 The idempotency check comes before almost everything

`create_transfer` (`src/state_machine.zig:3693`) checks the id before looking at accounts or balances:

```zig
if (t.flags.padding != 0) return .reserved_flag;

if (t.id == 0) return .id_must_not_be_zero;
if (t.id == math.maxInt(u128)) return .id_must_not_be_int_max;

switch (self.forest.grooves.transfers.get(t.id)) {
    .found_object => |e| return self.create_transfer_exists(t, &e),
    .found_orphaned => return .id_already_failed,
    .not_found => {},
}
```

The consequence: a retry of a transfer that already succeeded returns `exists`, **even if the world has changed since**. For example, the account may have been closed in the meantime. The state-dependent checks never run for a known id.

`create_transfer_exists` (`state_machine.zig:3971`) then compares the retry field by field, and returns the first mismatch:

```
flags → pending_id → timeout → debit_account_id → credit_account_id → amount
      → user_data_128 → user_data_64 → user_data_32 → ledger → code
all equal → .exists (with the ORIGINAL timestamp)
```

(For post/void transfers it goes on to `post_or_void_pending_transfer_exists` after `timeout`.)

- `exists` means "this exact transfer is already in the ledger". Most apps should treat it **exactly like `created`** (`create_transfers.md`, "`exists`").
- `exists_with_different_*` means "you reused an id for a different transfer". That is an application bug, and the codes say which field differs.
- Balancing transfers are a special case. The stored amount is the actual amount moved, so a retry is `exists` as long as the requested maximum is at least the stored amount (`if (t.amount < e.amount) return .exists_with_different_amount;`).

Each result is 16 bytes: `CreateTransferResult { timestamp: u64, status: u32, reserved: u32 }` (`src/tigerbeetle.zig:477`). For `created` and `exists` the timestamp is the transfer's commit timestamp. So a retry learns *when* the original committed.

### 3.3 Failed ids are burned when the failure depends on state

The subtle part: what if the first attempt **failed**?

- Some failures depend only on the request itself: `accounts_must_be_different`, `ledger_must_not_be_zero`, and so on. Retrying the same bytes always fails the same way. Fixing the bug and resubmitting with the same id is allowed.
- Some failures depend on database state at that moment. These are **transient**, listed in `CreateTransferStatus.transient()` (`src/tigerbeetle.zig:322`): `debit_account_not_found`, `credit_account_not_found`, `pending_transfer_not_found`, `exceeds_credits`, `exceeds_debits`, `debit_account_already_closed`, `credit_account_already_closed`.

If a transient failure didn't stick, a retry minutes later could *succeed*: the account gets funded, the client retries the "failed" payment, and the money moves long after the user saw "declined". One id would have two outcomes. So `transient_error` (`state_machine.zig:3189`) records the id as an **orphaned primary key**:

```zig
// Transfers that fail with transient codes cannot reuse the same `id`,
// ensuring strong idempotency guarantees.
// Once a transfer fails with a transient error, it must be retried
// with a different `id`.
.create_transfers, ... => if (result_status.transient()) {
    self.forest.grooves.transfers.insert_orphaned_primary_key(id);
},
```

Any later attempt with that id gets `id_already_failed`. The rule: **one id, one outcome, forever.** A genuinely new attempt (the user presses "pay" again) needs a new id. `create_accounts` errors are never transient, so accounts don't do this.

### 3.4 Linked chains and retries

`execute_create` treats any status other than `created` as a chain failure, **including `exists`** (`state_machine.zig:3091`; the test "linked accounts" in `state_machine_tests.zig:1526` labels it "Fail with .exists"). Retrying a chain `[X1 linked, X2]` that already committed returns:

```
X1 → exists
X2 → linked_event_failed      (never checked)
```

- My inference, not stated in the docs: chains are atomic, so if `X1` was only ever submitted inside this chain, `exists` on `X1` implies `X2` committed too. Confirm with `lookup_transfers` if in doubt.
- An edge case documented in a test comment (`state_machine_tests.zig:1178`): `exists` "always returns the timestamp of the original event. Even if the existing event was created within a linked chain and rolled back." For example, `{id=1 linked; id=1}` returns `linked_event_failed` and then `exists`, both with the same timestamp.

### 3.5 Gateways and batching

```
 phones/browsers (generate + persist ids)
        │  HTTPS, retries with same id
        ▼
 ┌───────────────┐  load balancer may send a retry to ANY gateway
 │  gateway 1..k │  stateless: auth, validation, cached ledger/code metadata,
 │  (TB client)  │  batching. No idempotency table.
 └──────┬────────┘
        │ one request = up to 8,189 transfers, one in flight per session
        ▼
   TigerBeetle cluster  ← the only place state lives
```

- **Why batch.** Consensus, replication and disk writes are paid once per request (Concepts 5 and 7). The docs compare it to a train versus cars: under light load, batches shrink by themselves and latency stays low (`performance.md`).
- **How the client batches.** A session has at most one request in flight. While it waits, the client keeps collecting events from all threads, then sends them together (`requests.md`, "Automatic Batching"). So share one client per gateway process across threads.
- **Batch limit, from the code.** The message size is 1 MiB with a 256-byte header, so the body is `1,048,320` bytes. `create_transfers` is multi-batch encoded, and the trailer for one batch is padded to one 128-byte element (`multi_batch.zig`, `trailer_total_size`). So `event_max = (1,048,320 − 128) / 128 = 8,189`. The test at `multi_batch.zig:655` pins `batch_size_max = 8189`.
- **Why batching is safe.** Batching mixes unrelated users' transfers in one request, but each event succeeds or fails independently unless linked. If a gateway dies mid-batch, the app retries through another gateway, the transfer lands in a different batch with different neighbours, and the id still makes the outcome unique.
- **Keep the gateway fast.** "Initiating a transfer should not require fetching metadata from the general purpose database" (`system-architecture.md`). Mappings such as "USD"→`ledger` and "refund"→`code` are append-only, so the gateway can cache them forever.

> ⚠️ **Docs vs code:** `requests.md` ("Idempotency") says the first create "will receive the `ok` result". The code returns `created` (`created = maxInt(u32)` in `CreateTransferStatus`). `ok` survives only as `deprecated_ok = 0`, from the old sparse result format.
>
> ⚠️ **Docs vs code:** `performance.md` says "up to 8,190 transfers per query" (as does the comment at `constants.zig:647`). `requests.md` says 8,189. 8,190 is the raw `1,048,320 / 128`. The multi-batch trailer costs one slot, so the real `create_transfers` limit is **8,189**.

### 3.6 Linking to the general-purpose database: `user_data` and "write last, read first"

TigerBeetle is the system of record for money. Names, emails, order details and so on live in a general-purpose (OLGP) database. The link between them is the `user_data` fields, each indexed for queries (`data-modeling.md`, "`user_data`"):

- `user_data_128`: who/what, for example the order id or user id in Postgres. Also a group id for related transfers (a currency exchange pair).
- `user_data_64`: when it happened in the real world (a second timestamp).
- `user_data_32`: where, for example a jurisdiction or locale.
- For privacy, store only an opaque key. Deleting the mapping in the OLGP database "forgets" the user (`debit-credit.md`).

`ARCHITECTURE.md` links a blog post, "The Write Last, Read First Rule", for how to order writes across the two databases.

> ⚠️ **Not verifiable in the clone.** The post itself isn't in the repository, only the link. As I understand the rule:
> - **Write** to the OLGP database first and to TigerBeetle **last**, so the TigerBeetle write is the commit point.
> - **Read** TigerBeetle **first**, and treat an OLGP row without a matching TigerBeetle object as not existing (an orphan from a failed attempt, safe to clean up).
>
> Check the post before relying on the details.

## 4. What it means for ashi

**Rust features that help:**
- **A validated newtype for ids**: `struct TransferId(u128)`, built only via `TryFrom<u128>` that rejects `0` and `u128::MAX`. Keep the raw `u128` in the wire struct, because ashi must *receive* bad ids in order to answer `id_must_not_be_zero`. `NonZeroU128` covers only half the rule and can't hold the invalid value on the wire.
- **`#[must_use]` result types and exhaustive `match`** force callers to handle `exists` and `id_already_failed` explicitly.
- **At the gateway, async is fine.** The gateway is stateless and IO-bound, so tokio plus axum suits it. Concept 3's "no async" rule applies to the core state machine, not to the edge.

**Pattern: the batcher.** Gateway HTTP handlers push `(Transfer, oneshot::Sender<Result>)` into a bounded channel. One batcher task owns the database client. When the in-flight request completes, it drains up to 8,189 queued events into the next request. No timers are needed, because the single in-flight request sets the rhythm. TigerBeetle's Rust client has its own small `oneshot.rs` for the same reply hand-off.

**Pattern: an idempotency table inside the state machine.** Store burned ids the way TigerBeetle does: an "orphaned" marker in the same primary-key index. Then one lookup answers created, exists or burned, and the check stays a single prefetchable key (Concept 10).

**Pitfalls specific to Rust (and HTTP stacks):**
- **New id per retry.** Retry middleware such as `reqwest-retry` or `tower::retry` re-sends the same body, which is good. But any helper that *generates* an idempotency key per attempt destroys the guarantee. The id must come from the persisted client record.
- **Random ids.** `uuid::Uuid::new_v4()` is the reflex choice in Rust, and it hurts LSM throughput. Prefer TBID layout, `ulid` (the same 48-bit time + 80-bit random split that inspired TBID), or UUIDv7 (`uuid` with the `v7` feature; it spends 6 bits on version and variant). All are time-ordered. Check that same-millisecond ids increase monotonically in whichever library you pick.
- **One global `Mutex` id generator** (as the TigerBeetle Rust client does) is fine for apps. For high-rate gateways, prefer a per-thread generator: each stays monotonic, and 80 random bits keep collisions negligible.
- **Generating ids at the gateway** is convenient and wrong. A gateway crash before the reply means the app retries with no id, and you get a duplicate.

**Crates worth evaluating:** `uuid` (v7) or `ulid` for ids, if not hand-rolled (the TBID generator is about 60 lines); `tokio`, `axum`, `tower` for the gateway. The core needs none.

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt** end-to-end client ids, the field-by-field `exists_with_different_*` codes and transient-id burning. **Adapt** by shipping a reference gateway and a sharper story for retrying linked chains (Q5).

## 5. Drill questions

### Q1. Recall

Who generates a transfer's `id`, and at what step must it be persisted? Draw the TBID bit layout. Name two id values TigerBeetle always rejects.

> _Your answer:_

### Q2. Mechanism

Why does a transfer that failed with `exceeds_credits` burn its id (`id_already_failed` on retry), while one that failed with `accounts_must_be_different` does not? What user-visible bug does burning prevent?

> _Your answer:_

### Q3. Back-of-envelope

A gateway shares one TigerBeetle client across threads. Each request takes 2 ms round trip, and there is one request in flight per session.

1. What is the throughput ceiling of one gateway with full batches?
2. Someone proposes one TigerBeetle client per end user for 10,000 concurrent users instead. What happens, given the production `clients_max`?

> _Your answer:_

### Q4. What breaks if…

…the gateway, not the app, generates the transfer `id` when the HTTP request arrives? Walk through a failure in which a customer is charged twice.

> _Your answer:_

### Q5. ashi design (open-ended)

Retrying a committed linked chain in TigerBeetle returns `exists` and then `linked_event_failed`. Should ashi keep this, or report `exists` for every event of an already-committed chain? What would ashi need to store or check to do the latter, and what could go wrong?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- The end application (phone or web page) generates the id, and persists it locally **before** submitting (step 3). It reuses the same id on every retry and after restarts.
- TBID: the top 48 bits are milliseconds since the Unix epoch; the low 80 bits are random, incremented within the same millisecond.
- Rejected ids: `0` (`id_must_not_be_zero`) and `2^128 − 1` (`id_must_not_be_int_max`).

**Q2.**
- `exceeds_credits` depends on the account's balance *at that moment*, so it is transient. Retrying later could succeed. A payment the user saw "declined" would then silently go through hours later, when the account happens to be funded, and the same id would have two outcomes.
- Burning the id (`insert_orphaned_primary_key`) fixes the outcome: retries get `id_already_failed`, and a deliberate new attempt needs a new id.
- `accounts_must_be_different` depends only on the request bytes, so a retry of the same bytes fails the same way. Not burning lets a developer fix the malformed request and resubmit it under the same id (`create_transfers.md`, "`id_already_failed`").

**Q3.**
1. `8,189 / 2 ms ≈ 4.1 million transfers/s` per gateway, as a ceiling from batching alone. In practice the cluster's execution rate is the limit, so gateways scale for HTTP handling and availability, not for TigerBeetle throughput.
2. `clients_max = 64` (`config.zig:228`). Registering client 65 evicts the session that "committed a request the longest time ago" (`sessions.md`). With 10,000 clients, sessions are evicted constantly, clients fail with `session evicted`, and batches are tiny, which wastes the per-request cost. So use a few clients with big batches.

**Q4.**
- The app sends `POST /pay` with no id. Gateway A assigns id `K1`, submits, and TigerBeetle commits `K1`.
- Gateway A crashes before replying (or the load balancer times out). The app, or the load balancer, retries.
- Gateway B receives what looks like a new request and assigns `K2`. TigerBeetle has never seen `K2`, so it creates a second transfer. The customer is charged twice.
- The key has to exist *before* the first hop that can fail ambiguously, which is the device. That is why the docs say the app generates and persists it.

**Q5.** Open-ended. Points to weigh:
- **Reporting `exists` for all events** needs a chain identity. Options: store a chain id in each transfer (spending `user_data` or a new field), or look up every id in the chain and check they all exist with matching fields. That costs N lookups, all prefetchable.
- **Risks:** a partial match (some ids exist, others don't) has no clear answer, and reporting `exists` there could hide a real bug. TigerBeetle's current rule is simple and deterministic, and leaves the inference to the app.
- **Middle ground:** keep execution semantics as they are, and add a client-library helper that, on `exists` + `linked_event_failed`, runs `lookup_transfers` and reports "chain already committed" only if every id matches.
- **Whatever ashi chooses**, the rule must be deterministic and part of the validation precedence (Concept 2, section 3.10).

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
