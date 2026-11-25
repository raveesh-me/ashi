# Concept 33: Strict serializability and what ACID means here

**Module:** M6 Operations · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/concepts/safety.md`, all sections: "Strict Serializability", "High Availability", "Storage Fault Tolerance", "Software Reliability", "Human Fallibility", "Is TigerBeetle ACID-compliant?", "Security"
- `../tigerbeetle/docs/coding/time.md`, section "Logical Time"
- `../tigerbeetle/docs/coding/requests.md`, sections "Batching Events" and "Guarantees"
- `../tigerbeetle/docs/coding/linked-events.md`
- `../tigerbeetle/docs/reference/sessions.md`, section "Guarantees"
- `../tigerbeetle/docs/operating/cluster.md`, section "Fault Tolerance"
- `../tigerbeetle/docs/ARCHITECTURE.md`, the storage-fault paragraph citing the Jepsen report (line 86–90) and "Embracing Concurrency"
- `../tigerbeetle/src/vsr/replica.zig`: `on_request` (1937), `ignore_request_message_backup` (6425), `commit_prefetch` snapshot choice (4764–4772), `primary_pipeline_prepare` (7307), the `state_machine.commit` call (5460)
- `../tigerbeetle/src/state_machine.zig`: `prepare_delta_nanoseconds` (1182), `commit` (2538), `execute_create` (2976)
- `../tigerbeetle/src/vsr/client.zig` (74, 740), `../tigerbeetle/src/vsr/message_header.zig` (570, 706)
- `../tigerbeetle/src/vsr.zig`: `quorums` (1212)
- `../tigerbeetle/src/config.zig` (155–157, 218–229), `../tigerbeetle/src/constants.zig` (776–795), `../tigerbeetle/build.zig` (200–202)

## 1. The problem

A ledger answers one question for its users: "what happened, and in what order?" If the answer can differ depending on who asks, or when, money gets lost.

Two failures are typical:

- **Anomalies from weak isolation.** Isolation is how much concurrent transactions can see of each other. Most databases default to a weak level, such as Postgres's `READ COMMITTED`. Under it, two transfers can both read a balance of 100, both decide 80 can be withdrawn, and both commit. That is a double spend. The fix exists (`SERIALIZABLE`), but someone has to know to turn it on.
- **Stale reads in a replicated system.** A read is *stale* if it misses a write that had already been acknowledged. A client gets "transfer committed", tells a second service, and the second service reads the balance from a lagging replica. The transfer is not there. To the business, the money has vanished.

TigerBeetle's docs put it bluntly: "The easiest way to lose data is by incorrectly using the database, by misconfiguring (or just misunderstanding) its isolation level" (`safety.md`, "Strict Serializability").

This lesson pins down the vocabulary, then shows how TigerBeetle gets the strongest guarantee almost for free from earlier concepts.

## 2. The obvious solution, and why it fails

### First, the three terms

| Term | Applies to | Ordering promise | Respects real time? |
|---|---|---|---|
| **Serializable** | multi-object transactions | the result equals *some* serial (one-at-a-time) order | no |
| **Linearizable** | single-object operations | each operation appears to take effect at one instant between its call and its return | yes |
| **Strictly serializable** | multi-object transactions | a serial order that *also* respects real time | yes |

"Respects real time" means: if transaction A *finished* (the client got its reply) before transaction B *started*, then A comes before B in the serial order. Strict serializability is serializability plus that rule. You can also read it as linearizability where each "operation" is a whole transaction over many objects.

### A history that is serializable but not strictly serializable

```
real time ─────────────────────────────────────────────────────────►

Alice:  [ T1: credit account X +100 ]──reply "ok"
                                              │  Alice phones Bob: "it's there"
Bob:                                          └──► [ T2: read X ] → 0
```

- **Serializable?** Yes. The serial order `T2, T1` explains every result: Bob read first, saw 0, then Alice's credit landed.
- **Strictly serializable?** No. T1 returned before T2 started, so T1 must come first. Bob must see 100.

Serializability alone lets the database "time travel" backwards across an acknowledged write. Most replicated databases can produce exactly this history. Two common ways:

1. **Follower reads.** The read is served by a replica that has not yet applied T1.
2. **Leader-local reads.** A deposed leader, cut off by a network partition, still believes it leads and answers from its own (now old) state.

### The obvious fixes, and their costs

- **Offer a `SERIALIZABLE` knob.** The anomaly still exists for everyone who doesn't set it, and the knob doesn't fix stale replica reads anyway.
- **Pessimistic locking (two-phase locking).** Correct, but the locks on hot accounts become the bottleneck from Concept 1.
- **Leader leases.** The leader serves reads locally for a time window that other replicas promise not to elect someone else. This is fast, but its safety rests on bounded clock drift. If a clock jumps (a VM pause, a bad NTP step), two leaders can both serve reads, and one is stale.

Every fix either has a configuration trap, a throughput cost, or a hidden clock assumption.

## 3. What TigerBeetle does

### 3.1 One isolation level, built from two earlier ideas

TigerBeetle "intentionally supports only the strictest possible isolation level -- **strict serializability**. All transfers are executed one-by-one, on a single core" (`safety.md:15–18`). There is no knob to get wrong.

Strict serializability falls out of two mechanisms you have already met:

1. **A total order.** Consensus (Viewstamped Replication, Concept 21) assigns every request an `op` number. The op numbers form one log, the same on every replica.
2. **Serial execution.** One thread applies the log in op order (Concept 6). Nothing ever runs concurrently against the state, so there are no interleavings to reason about.

```
 clients            primary                       every replica
 ───────            ───────                       ─────────────
 req A ──┐                                        state machine, 1 thread
 req B ──┼──► assign op 101, 102, 103 ──► log:   ┌──────────────────────────┐
 req C ──┘    (replicate, wait for quorum)        │ op 101 │ op 102 │ op 103 │
                                                  └───┬────┴───┬────┴───┬────┘
                                                      ▼        ▼        ▼
                                              commit(101) → commit(102) → commit(103)
                                              (reply sent only after commit)
```

- **Serializable:** the serial order *is* the op order.
- **Real time respected:** a reply is sent only after its op commits. Any request that arrives later gets a higher op number, so it executes after. That holds across view changes too: a new primary's log contains every committed op, and it appends new ops after them.

`docs/ARCHITECTURE.md` ("Embracing Concurrency") stresses that only *execution* is serial. Replication, prefetch IO (Concept 10) and compaction all run concurrently around it: "_Execution_ of requests has to be sequential but replication can be concurrent."

### 3.2 Reads go through consensus too

This is the part most databases skip. `safety.md:144–146`: "stale reads are not possible since all operations (not only writes, but also reads) go through the global consensus protocol."

The code confirms it. There is no read-only fast path:

- **Backups never answer new requests.** `ignore_request_message_backup` (`src/vsr/replica.zig:6425`) forwards any new request to the primary (line 6436, "forwarding new request to primary"). A backup only answers a *duplicate* request, by resending the stored reply of a request that already committed (`on_request_repeat_reply`, line 6655). That is a replay, not a fresh read.
- **The primary only takes requests when caught up.** `on_request` asserts it at entry (`replica.zig:1937–1944`):

  ```zig
  assert(self.status == .normal);
  assert(self.primary());
  assert(self.syncing == .idle);
  assert(self.commit_min == self.commit_max);
  ```
- **Reads are prepared like writes.** `primary_pipeline_prepare` (`replica.zig:7307`) switches on the operation. Only VSR-internal operations (`register`, `reconfigure`, `upgrade`, `noop`) are special. Every state-machine operation, read or write, falls into `else => self.state_machine.prepare(...)` (line 7385), gets an op number and a timestamp, and is replicated and journaled.
- **Reads are executed in `commit`.** `StateMachine.commit` (`src/state_machine.zig:2538`) handles `.lookup_accounts`, `.lookup_transfers`, `.get_account_transfers`, `.query_accounts` and the rest in the same switch as `.create_transfers`. The replica calls it at `replica.zig:5460` for every committed prepare.
- **Reads see exactly their place in the log.** The LSM snapshot used for prefetch is the op number itself (`replica.zig:4764–4772`): "we use the op number as the snapshot, as it is guaranteed to be the latest snapshot." (After a crash-recovery the snapshot is adjusted to the checkpoint trigger + 1, for LSM reasons, see Concept 18.)
- **Reads consume no timestamps for events.** `prepare_delta_nanoseconds` (`state_machine.zig:1182`) returns `0` for every lookup and query and `batch.len / @sizeOf(Transfer)` for `create_transfers`. A read still gets its own op and a strictly larger prepare timestamp (`replica.zig` asserts `prepare_timestamp > commit_timestamp`), but it creates no objects.

**Why this is safe without leases.** A deposed primary cannot answer a read, because answering means committing, and committing needs a replication quorum. The quorums are built to intersect (`src/vsr.zig:1212`, `quorums`):

```zig
const quorum_replication = if (replica_count == 2) 2 else @min(
    constants.quorum_replication_max,
    stdx.div_ceil(replica_count, 2),
);
...
const quorum_view_change =
    if (replica_count == 2) 2 else replica_count - quorum_replication + 1;
...
assert(quorum_view_change + quorum_replication > replica_count);
```

With 6 replicas and `quorum_replication_max: u8 = 3` (`src/config.zig:157`): replication quorum 3, view-change quorum 4 (Concept 22). Any new view has 4 replicas that have moved on. They don't ack the old view's prepares (`on_prepare` diverts `message.header.view < self.view` to repair, `replica.zig:2059`; `on_prepare_ok` ignores older views, line 6108), so the old primary can gather at most 2 acks. It can't commit, so it can't reply. No clock assumption needed. That is why `safety.md:39` can say TigerBeetle "does not depend on synchronized system clocks, does not use leader leases".

**The price.** A read costs a full replication round trip plus a WAL write, and it takes a slot in the pipeline (`pipeline_prepare_queue_max: u32 = 8`, `config.zig:155`). Batching pays for it: one `lookup_accounts` request carries up to 8189 ids (`docs/coding/requests.md`, "Batching Events").

### 3.3 Per-session guarantees and hash chains

`docs/reference/sessions.md` ("Guarantees") spells out what one client session sees:

- At most one in-flight request per session.
- **Read your own writes**: reads after a write observe it.
- Writes observed in cluster order; `debits_posted` and `credits_posted` never decrease.
- No uncommitted updates; no broken invariants (e.g. `credits_must_not_exceed_debits`, `linked`).
- **Across sessions, replies can arrive out of order.** The client whose request committed first might get its reply later. Order is defined by commit, not by reply arrival.

Hash chains (Concept 15) make the ordering checkable. Each request carries the checksum of the previous reply in `parent`, and each reply the checksum of its request (`src/vsr/client.zig:74`: "We hash-chain request/reply checksums to verify linearizability within a client session"). Prepares chain to the previous prepare (`src/vsr/message_header.zig:706`: "This provides a strong guarantee for linearizability across our distributed log of prepares").

Clients also **hedge**: each request goes to the primary *and* one random backup (`client.zig:740`), so a broken client→primary link doesn't stall the request. The backup just forwards it. Hedging improves availability without opening a stale-read path.

### 3.4 The ACID mapping

`safety.md` ("Is TigerBeetle ACID-compliant?") answers "Yes" and maps each letter. Each one has a protocol meaning and a stronger *financial* meaning.

| Letter | Protocol-level guarantee | Financial-level guarantee |
|---|---|---|
| **A**tomicity | an op is committed only after it is in a quorum of WALs; crash → replay the WAL from the last checkpoint | `flags.linked` chains succeed or fail together |
| **C**onsistency | strict serializability; no stale reads | double-entry: money is never created or destroyed; history is immutable |
| **I**solation | every request and every event executes serially, one after another | no locks needed, even within a batch |
| **D**urability | explicit storage fault model; data lost only if corrupt on *every* replica, and then the cluster halts | — |

**Atomicity, precisely.** Two levels, which are easy to confuse (`docs/coding/requests.md`, "Guarantees"):

- "All events within a request batch are committed, or none are."
- But "this does not mean that all of the events in a batch will succeed". Events succeed or fail *independently* unless linked.

Linked chains are implemented with undo scopes. `execute_create` (`state_machine.zig:2976`) opens a scope on the first linked event and rolls it back if any event in the chain fails, marking the rest `linked_event_failed`:

```zig
if (event.flags.linked) {
    if (chain == null) {
        chain = index;
        assert(chain_broken == false);
        self.scope_open(operation);
    }
    if (index == events.len - 1) break :result .{
        .linked_event_chain_open,
        timestamp_event,
    };
}
if (chain_broken) break :result .{ .linked_event_failed, timestamp_event };
```

**The "C" nuance.** "Consistency" means three different things in the literature:

1. **ACID's C** (the original meaning): each transaction preserves the application's invariants. Classically this is the *application's* job; the database only helps via constraints.
2. **CAP's C**: linearizability, a replication property.
3. **"Consistency models"**: the family of isolation and ordering guarantees.

`safety.md`'s "Consistency" paragraph opens with meanings 2 and 3 ("strict serializability", "no stale reads"). Its second paragraph is the true ACID C: the double-entry API enforces the invariant "money cannot be created or destroyed". That is unusual. In a generic database, the "debits equal credits" invariant lives in application code, and a bug there breaks it. In TigerBeetle the invariant is the schema (Concept 2), so no client bug can violate it.

> ⚠️ **Terminology, not a code conflict:** `safety.md` uses "Consistency" for both isolation (strict serializability) and invariant preservation. Keep them apart when you write ashi's docs.

**Isolation without locks.** `safety.md:156–158`: "the use of batching and serial execution means that TigerBeetle can also provide this level of isolation optimally, without the cost of locks." Within a batch, "events within a request do not interleave with events from other requests" (`requests.md`).

**Durability as the keystone.** `safety.md:162–163`: Durability is "the only letter in ACID whose loss undoes the others." TigerBeetle replaces the crash-consistency model with an explicit storage fault model (Concept 14) because Fsyncgate and Protocol-Aware Recovery showed that the old model loses data in practice. A record must be corrupt on all replicas to be lost, and even then "the system would safely halt" rather than serve wrong data.

### 3.5 The rest of `safety.md`, briefly

**High availability** (Concepts 21–23, 26):

- VSR gives correct, automatic failover. "Consensus proper needs only be engaged during actual failover"; in normal operation the cost is just replication.
- Leader-based timestamping with a fault-tolerant "cluster time" (Concept 26), so apps only deal in relative time for timeouts.
- Recommended deployment: 6 replicas across 3 cloud providers, 2 per provider. With flexible quorums this survives the loss of a whole provider. `docs/operating/cluster.md` gives the numbers: 4/6 to elect a new primary; available with ≥3/6 if the primary survives, ≥4/6 if it must be replaced. `replicas_max = 6` (`src/constants.zig:24`).
- Gray failure (slow disk, lossy NIC) is detected and routed around by adjusting replication topology (Concept 23).

**Storage fault tolerance** (Concepts 14–17, 24–25):

- The disk *will* fail. The docs quote annual rates: silent corruption on 0.031% of SSDs and 1.4% of enterprise HDDs; misdirected IO on 0.023% of SSDs and 0.466% of nearline HDDs.
- Everything is immutable, checksummed and hash-chained; latent sector errors are repaired from other replicas without an operator.
- Protocol-Aware Recovery keeps the cluster available "unless the data gets corrupted on every single replica."
- Minimal software between TigerBeetle and the disk: own page cache, `O_DIRECT`, works on a raw block device.

**Software reliability:** Zig, TigerStyle (Concept 30), static allocation (Concept 11), the VOPR (Concept 29).

**Human fallibility.** Operators make mistakes, so the design removes chances to make them:

- minimal surface area and little configurability;
- one isolation level;
- automatic, atomic upgrades, "guaranteeing that each transfer is applied by only a single version of code" (Concept 32);
- "always runs with online verification on".

> ⚠️ **Docs vs code, partial.** "Online verification" is not defined in the docs. Production binaries are built with `-Drelease` (`src/scripts/release.zig:282`), which selects `ReleaseSafe` (`build.zig:89`), so ordinary `assert`s and bounds checks stay on. But the *extra* costly checks gated by `constants.verify` are **off** in release builds: `build.zig:200–202` sets `config_verify` to `options.mode == .Debug` unless overridden, and `config.zig:290` overwrites the `.verify = true` in `default_production`. The grid scrubber (background re-reading of blocks, default cycle 180 days, `config.zig:138`) is also on. So "online verification" is true for assertions and scrubbing, but not for `constants.verify` checks. The code wins.

**Security** (`safety.md:179–193`):

- TigerBeetle is a trusted component in a trusted environment. It has **no permission system**; the application must do access control.
- Its attack surface is small: fixed-size integer data, no (de)serialization, no user-generated strings, extensive fuzzing.
- `io_uring` has had kernel exploits and is disabled in some sandboxes. The docs argue TigerBeetle's use is safe because it only handles trusted integer data.

### 3.6 Jepsen

The docs cite Jepsen exactly once as evidence: `docs/ARCHITECTURE.md:86–90` says TigerBeetle "guarantees durability and availability even in the presence of latent sector errors and helical corruption", linking `https://jepsen.io/analyses/tigerbeetle-0.16.11#disk-faults`. (`sessions.md` also links Jepsen's *definition* of read-your-writes, which is not an analysis.)

- "Helical corruption" is not defined anywhere in the clone. My reading, unverified: corruption that hits different places on different replicas, so every piece of data still survives intact somewhere.
- The clone contains no summary of the report's other findings. **Unverified, from memory:** Jepsen's analysis of 0.16.11 broadly upheld the strict-serializability and fault-tolerance claims and reported a number of bugs, several in client libraries and query paths rather than in consensus. Read the report itself before quoting any detail.

### 3.7 How TigerBeetle checks the claim in tests

The VOPR (Concept 29) checks this guarantee continuously:

- `src/testing/cluster/state_checker.zig` tracks the canonical sequence of commits and checks that every replica commits the same prepare at each op.
- `src/state_machine/auditor.zig` "constructs the expected state of its corresponding StateMachine from requests and replies. It validates replies against its local state", and "expects replies in ascending commit order". It is a model checker for the reply stream.

## 4. What it means for ashi

**Strict serializability is cheap if the architecture is right.** It falls out of: one totally ordered log + one thread applying it + replies only after commit + reads through the log. Bolt it on later and it gets expensive.

**Rust features that help:**

- **`&mut self` as the single-writer proof.** Make `StateMachine::commit(&mut self, op: u64, ...)` the only mutator, owned by one thread. The borrow checker then rules out concurrent mutation. No `Arc<Mutex<State>>` anywhere on the data path.
- **Type-state for commit order.** A `PreparedOp { op: u64, ... }` that can only be built by the consensus layer, consumed by `commit`, plus a `debug_assert_eq!(op, self.commit_min + 1)` style check (keep it as `assert!`; it is O(1), Concept 30).
- **No isolation-level enum in the API.** If it doesn't exist it can't be misconfigured.
- **Undo scopes for linked chains.** Model a scope as a guard struct whose `Drop` is *not* the rollback (drop must not hide a decision). Make `rollback()` and `commit()` explicit methods that consume the guard; add a `#[must_use]`.

**Rust pitfalls:**

- **Tokio makes concurrency the default.** A `tokio::spawn` per request plus a shared state lock quietly turns ashi into "serializable at best". Keep the state machine on one dedicated thread and feed it through a channel or a ring buffer (Concept 12).
- **"Fast read" temptation.** Serving `lookup_accounts` from an `RwLock` snapshot on another thread *reintroduces* the stale-read history from section 2, because the snapshot may lag an acknowledged commit.
- **`HashMap` iteration order** is randomized per process. Any reply that iterates a `std::collections::HashMap` differs across replicas and breaks determinism (Concept 20). Use ordered structures or a fixed hasher.

**Design options for reads:**

| Option | Latency | Clock assumption | Stale-read risk |
|---|---|---|---|
| Through the log (TigerBeetle) | 1 replication RTT + WAL write | none | none |
| Quorum-confirmed read, no log append (Raft's "ReadIndex" idea) | 1 quorum RTT, no disk write | none | none if done right; more code paths to test |
| Leader lease | local | bounded drift | yes, if a clock misbehaves |
| Follower read | local | none | yes, by design |

The middle row is the only plausible improvement. It saves the WAL write and a pipeline slot, at the cost of a second code path the VOPR must cover.

**Crates worth evaluating (tests only, since TigerBeetle has zero dependencies, Concept 13):**

- `stateright`: a Rust model checker that ships a linearizability tester. Good for checking the reply history of a small model. It is not a replacement for a VOPR-style simulator.
- Jepsen itself (Clojure) with Elle (transactional anomalies) and Knossos (linearizability) can black-box test an ashi cluster later. Heavy, but it is the external standard.
- Hand-written auditor (like `auditor.zig`): probably the best fit, since the debit/credit model is small.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt** strict serializability as the only level with reads through the log; maybe **adapt** later with quorum-confirmed reads if measurements show reads dominate latency, because both keep the no-clock-assumption property.

## 5. Drill questions

### Q1. Recall

Define serializable, linearizable and strictly serializable in one sentence each. Which of the three does a single-object key-value store with follower reads usually *fail*, and why?

> _Your answer:_

### Q2. Mechanism

Explain why a TigerBeetle primary that has been partitioned away from the cluster cannot return a stale `lookup_accounts` result, even though it never checks a clock or a lease. Name the two quorum sizes for a 6-replica cluster and the inequality that makes it work.

> _Your answer:_

### Q3. Back-of-envelope

A 6-replica cluster spans 3 sites, 2 replicas per site. Inter-site round trip is 2 ms, intra-site 0.1 ms, and a WAL write takes 0.05 ms. The client sits in the primary's site.

- Roughly what latency does a single `lookup_accounts` request see, ignoring CPU time?
- With `pipeline_prepare_queue_max = 8` and 8189 lookups per request, what is the ceiling on lookups per second that consensus alone allows?
- What would a leader-lease read cost, and what does it assume?

> _Your answer:_

### Q4. What breaks if…

…ashi adds an optimization: backups answer `lookup_accounts` from their own state if they are "caught up within 1 ms". Write a concrete history (clients, replies, real-time arrows) that this permits and that strict serializability forbids. Which `sessions.md` guarantee could still hold, and which breaks?

> _Your answer:_

### Q5. ashi design (open-ended)

`safety.md` says TigerBeetle is ACID, and that its "C" includes "money cannot be created or destroyed". Which invariants should ashi enforce *inside* the state machine, and which, if any, would you leave to applications? Where would you draw the line, and how would you test it?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- *Serializable:* the outcome of concurrent transactions equals some one-at-a-time order of them, with no promise about which order.
- *Linearizable:* each single-object operation appears to take effect atomically at some instant between its invocation and its response, so the order respects real time.
- *Strictly serializable:* transactions over many objects appear to execute one at a time, in an order consistent with real time. Serializable plus linearizable.
- A store with follower reads fails **linearizability** (and so strict serializability). A follower may not yet have applied a write that was already acknowledged, so a read that starts after the write finishes can miss it.

**Q2.**
- To reply to any request, including a read, the primary must commit it. Committing needs `quorum_replication` prepare_oks for that op in its view.
- For 6 replicas: `quorum_replication = min(3, ceil(6/2)) = 3`; `quorum_view_change = 6 - 3 + 1 = 4` (`src/vsr.zig:1212`).
- `quorum_view_change + quorum_replication > replica_count` (4 + 3 > 6, asserted in the code). Any newer view was started by 4 replicas, and they ignore the old view's prepares. Only 2 replicas remain to ack the old primary, fewer than 3. So the old primary can never commit, and never replies.
- No clock enters the argument. That's why TigerBeetle needs no leader leases.

**Q3.**
- *Latency:* client→primary ≈ 0.1 ms RTT. The commit quorum of 3 = the primary (0.05 ms write) + its same-site peer (0.1 ms + 0.05 ms) + the fastest remote replica (2 ms + 0.05 ms). The remote ack dominates: ≈ 2.05 ms. Total ≈ 2.2 ms. Note that a quorum of 3 out of 6 forces one remote ack, because each site holds only 2 replicas.
- *Ceiling:* 8 in flight × 8189 ≈ 65,500 lookups per ~2.05 ms ≈ 32 million lookups/s. In reality, CPU, prefetch IO and the reply size cap this much lower. The point is that batching makes the consensus cost of reads a latency cost, not a throughput cost.
- *Lease read:* ≈ 0.1 ms (the client's round trip only). It assumes clocks drift less than the lease margin. A paused VM or a clock step can create two "leaders" serving reads, one stale.

**Q4.**
- History: client A sends `create_transfers` crediting X; it commits at op 500 and A gets "ok" at t=10. A tells client B. At t=11, B's `lookup_accounts(X)` hits a backup that has only applied up to op 499 ("within 1 ms" by its own estimate). B sees the old balance. A finished before B started, yet B is ordered before A. Strictly serializable: forbidden. Serializable: allowed (order `B, A`).
- A per-session guarantee like "read your own writes" breaks too unless the backup checks it has applied the session's last op. "Never observes uncommitted updates" can still hold, since backups only apply committed ops. "Monotonic `debits_posted`/`credits_posted`" breaks if a session's consecutive reads land on different backups at different lags.
- "Caught up within 1 ms" also smuggles in a clock assumption that the protocol otherwise avoids.

**Q5.** (Open-ended; a strong answer covers these points.)
- *Inside:* conservation of value (every transfer debits one account and credits another by the same amount on the same ledger), balance limits (`debits_must_not_exceed_credits` style flags), immutability of history, uniqueness of ids (idempotency, Concept 4), atomic linked chains, two-phase pending/post/void rules. These are cheap per event, local to the accounts touched, and catastrophic if broken.
- *Outside:* anything needing strings, cross-system data, or policy that changes often: KYC, fraud scoring, account naming, authorization (TigerBeetle has no permission system either).
- *Line:* enforce an invariant inside if it is (a) expressible over fixed-size fields of the accounts and transfers in the batch, (b) must hold for every client, and (c) can be checked in O(1) per event.
- *Testing:* a reference model (auditor) that replays requests and replies and checks every invariant after every op, run under a deterministic simulator with faults (Concepts 29, 31), plus a sum-of-balances-per-ledger assertion.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
