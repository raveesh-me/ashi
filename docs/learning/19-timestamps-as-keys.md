# Concept 19: Timestamps as primary keys

**Module:** M3 Storage · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Time" and the storage paragraph under "Replicated State Machine" ("transfer objects are stored in a tree sorted by a unique timestamp")
- `../tigerbeetle/docs/coding/time.md` (all sections)
- `../tigerbeetle/docs/reference/transfer.md`, sections `flags.imported` and `timestamp`
- `../tigerbeetle/docs/reference/account.md`, sections `flags.imported` and `timestamp`
- `../tigerbeetle/docs/coding/data-modeling.md`, sections on choosing an `id` and "TigerBeetle Time-Based Identifiers"
- `../tigerbeetle/src/vsr/replica.zig:1966-1983` (`on_request`), `:7354-7426` (`primary_pipeline_prepare`), `:9753-9761` (view change), `:5460-5472` (commit)
- `../tigerbeetle/src/state_machine.zig:1145-1215` (`prepare`, `prepare_delta_nanoseconds`), `:2676-2705` (`execute_multi_batch`), `:2976-3080` (`execute_create`), `:3587-3662` (`create_account`), `:3774-3815` (`create_transfer` import checks), `:3914-3917`, `:5275-5278`
- `../tigerbeetle/src/lsm/timestamp_range.zig`, `groove.zig:25-48` and `:961-1100` and `:1509-1700`, `unique_key.zig`, `composite_key.zig`, `scan_builder.zig:31-36`
- `../tigerbeetle/src/tigerbeetle.zig:465-487` (result structs carry `timestamp`)

## 1. The problem

Every stored object needs a **primary key**: the key its storage tree is sorted by. The natural candidate is the object's `id`. That is a `u128` the **client** chooses, used for idempotency (Concept 4).

But a client-chosen key is a poor storage key:

- **No locality.** If ids are random, consecutive transfers land all over the key space. Every flush to disk overlaps every existing table, so compaction must rewrite everything (Concept 18).
- **No order.** An id tells you nothing about *when* something happened. "Transfers for account X between 09:00 and 10:00" needs a separate time index.
- **Wide pointers.** Every secondary index entry has to point back to the object. A 16-byte id makes every index entry wider than an 8-byte integer would.

A ledger also needs **time itself**: pending transfers time out, and auditors want to know when each event happened (`time.md`, "Physical Time").

## 2. The obvious solution, and why it fails

**Option A: key objects by `id`, and store a `created_at` column.**
- With random UUIDv4 ids, a bar of 262,080 transfers (Concept 18) spans almost the whole u128 range. That table overlaps every table in level 0, every level-0 table overlaps up to 8 in level 1, and so on. No table ever qualifies for move-table, so write amplification is at its worst.
- `created_at` from each server's wall clock is not unique, can go backwards (NTP step), and differs between replicas.

**Option B: an auto-increment sequence.** It is unique and ordered, but carries no wall-clock meaning. You would still need a second field for time, and a second index to query by time.

**Option C: each replica stamps events with its own clock.** Replicas then compute different state from the same log. That breaks the replicated state machine (Concept 20).

What TigerBeetle wants is a single `u64` that is unique, strictly increasing, means real time, and is identical on every replica.

## 3. What TigerBeetle does

> "Every object in TigerBeetle has a globally unique `u64` creation timestamp which plays the role of synthetic primary key." (`ARCHITECTURE.md`, "Time")

A **synthetic key** is a key the database invents, as opposed to a **natural key** (like `id`) that comes from the user's data.

### 3.1 Where a batch's timestamp comes from

Only the **primary** reads a clock for state. It reads it once per request, and also when deciding whether a `pulse` is needed (`replica.zig:3971-3979`).

1. **On request** (`replica.zig:1966-1983`): the primary asks for `self.clock.realtime_synchronized()`. That is wall-clock time clamped to the cluster's agreed time window (Concept 26). If the clock is not synchronized, the request is **dropped**: `"on_request: dropping (clock not synchronized)"`.
2. **On prepare** (`replica.zig:7354-7365`), the primary makes time monotonic:

```zig
// src/vsr/replica.zig:7354-7365
// Guard against the wall clock going backwards by taking the max with timestamps
// issued:
self.state_machine.prepare_timestamp = @max(
    // The cluster `commit_timestamp` may be ahead of our `prepare_timestamp` because
    // this may be our first prepare as a recently elected primary:
    @max(
        self.state_machine.prepare_timestamp,
        self.state_machine.commit_timestamp,
    ) + 1,
    @as(u64, @intCast(request.realtime)),
);
```

3. Then `state_machine.prepare(...)` **reserves one nanosecond per event** (`state_machine.zig:1145-1177`). It adds `prepare_delta_nanoseconds`, which is the event count for create operations and 0 for lookups and queries (`:1182-1215`). A `pulse` reserves `batch_max.create_transfers` nanoseconds, the maximum number of transfers one pulse can expire.
4. The result is written into the prepare's header as `timestamp` (`replica.zig:7419-7426`). It is the **highest** timestamp in the batch.

**Backups never consult their own clock** for state. They execute with `prepare.header.timestamp` (`replica.zig:5460-5463`). This is how a time-dependent state machine stays deterministic (Concept 20). `ARCHITECTURE.md` puts it this way: "the TigerBeetle primary injects a specific timestamp into the logic of the state machine when it converts a request to a prepare."

**Across view changes**, a new primary first raises its `prepare_timestamp` to the highest timestamp seen in the view-change quorum (`replica.zig:9753-9761`). So it never reissues a timestamp, even if its own clock is behind.

### 3.2 Per-event timestamps inside a batch

The header carries one number. Each event gets its own by counting back from it:

```zig
// src/state_machine.zig:3005
const timestamp_event = timestamp - events.len + index + 1;
```

```
header.timestamp = T, batch of 4 events:

  event index:      0        1        2        3
  timestamp:      T − 3    T − 2    T − 1      T
```

- **Multi-batch requests:** `execute_multi_batch` (`state_machine.zig:2694-2705`) starts at `timestamp − total_delta`. It then advances by each sub-batch's event count, so each sub-batch gets its own contiguous slice.
- **Every event uses up a timestamp, even one that fails.** The result reports `timestamp_event` for failures, and the actual object's timestamp for `created` or `exists` (`state_machine.zig:3072-3078`). The result structs carry the timestamp to the client (`tigerbeetle.zig:465-487`).
- **Expiries.** Expiring pending transfers in a `pulse` uses the same formula (`state_machine.zig:5275`). Each expiry gets a fresh timestamp for its account-event record.

So, as `time.md` says: "A transfer that is created before another transfer is guaranteed to have an earlier `timestamp` (even if they were created in the same request)."

**Monotonicity is asserted, not hoped for:**
- `assert(timestamp > self.commit_timestamp or self.aof_recovery)` in `commit` (`state_machine.zig:2550`).
- After each commit the replica checks and advances `commit_timestamp` (`replica.zig:5470-5472`):

```zig
assert(self.state_machine.commit_timestamp <= prepare.header.timestamp or
    self.aof_recovery);
self.state_machine.commit_timestamp = prepare.header.timestamp;
```

**Range and the tombstone bit.** Timestamps are nanoseconds since the UNIX epoch, capped one bit short of `u64`:

```zig
// src/lsm/timestamp_range.zig:3-11
pub const TimestampRange = struct {
    /// The minimum timestamp allowed (inclusive).
    pub const timestamp_min = 1;

    /// The maximum timestamp allowed (inclusive).
    /// It is `maxInt(u63)` because the most significant bit of the `u64` timestamp
    /// is used as the tombstone flag.
    pub const timestamp_max = std.math.maxInt(u63);
```

`0` is never a valid timestamp. That frees it as a sentinel: "the client didn't set it", or "orphaned id" (§3.4).

### 3.3 The object tree is sorted by timestamp

A groove refuses to compile unless the object has a `u64 timestamp` field (`groove.zig:26-27`, `:201-202`). Its object tree uses that field as the key (`groove.zig:385-399`):

```zig
// src/lsm/groove.zig:29-31
inline fn key_from_value(value: *const Object) u64 {
    return value.timestamp & ~@as(u64, tombstone_bit);
}
```

Every other tree in the groove stores the timestamp as its "pointer" to the object:

| Tree | Stored value | Size | Sorted by |
|---|---|---|---|
| Object tree | the object | 128 B | `timestamp` |
| `id` unique tree | `{id: u128, timestamp: u64, padding: u64}` | 32 B | `id` |
| index on a u64-or-smaller field (e.g. `ledger`) | `{field: u64, timestamp: u64}` | 16 B | `(field, timestamp)` |
| index on a u128 field (e.g. `debit_account_id`) | `{field: u128, timestamp: u64, padding: u64}` | 32 B | `(field, timestamp)` |
| flag index (e.g. `imported`) | `{timestamp: u64}` | 8 B | `timestamp` |

Layouts: `unique_key.zig:13-58`, `composite_key.zig:12-75`.

`ARCHITECTURE.md` ("LSM") gives the id-tree size directly: "for id tree the size of value is 32 bytes (`u128` id, `u64` timestamp, `u64` padding)."

Why the timestamp is a good pointer:
- It is **8 bytes**, not 16. A `ledger` index entry is 16 B with a timestamp. With a `u128` id it would need 24 B, padded to 32.
- It makes every secondary index entry **unique**. Two transfers with the same `ledger` still have distinct `(ledger, timestamp)` keys.
- Within one prefix, entries come out **in time order**. That is what lets range scans and zig-zag intersections work (Concept 18, §3.11).

### 3.4 Looking up by `id`

For accounts and transfers, `primary_key = "id"` (`state_machine.zig:405`, `:445`). So the in-memory **objects cache** is keyed by id, while the object **tree** is keyed by timestamp. A lookup in `prefetch` goes:

```
lookup transfer id = X
   │
   ├─1─► objects_cache.get_or_tombstone(X)          hit → done (no IO)
   │       (invariant: anything in the mutable table is also in this cache, groove.zig:687-688)
   │
   ├─2─► id tree: key_range_contains(X)?            no  → not found (no IO)
   │
   ├─3─► id tree: lookup X in levels                 → {id: X, timestamp: t}
   │       timestamp == 0  → "orphaned id": failed before, can't be reused
   │       tombstone       → not found
   │
   └─4─► object tree: lookup t                      → Transfer → insert into objects_cache
```

This is `prefetch_enqueue` (`groove.zig:961-1100`), then `PrefetchWorker.lookup_start_next` (`:1509-1576`), then the unique-key callback (`:1578-1668`), then `lookup_by_timestamp` (`:1670`). In the worst case, a cold lookup by id costs **two** tree lookups: id → timestamp, then timestamp → object.

**Orphaned ids.** Transfers set `primary_key_orphaned = true` (`state_machine.zig:446`). When a transfer fails with a *transient* error, its id is written to the id tree with `timestamp = 0` (`groove.zig:1882-1898`, called from `state_machine.zig:3222`). A retry then gets `id_already_failed` (`state_machine.zig:3710`). This keeps idempotency strong (Concept 4, Concept 27). It works only because 0 is not a valid timestamp.

### 3.5 Why this gives good locality

**Object trees become append-only logs.**
- New objects always carry the largest timestamp so far. So each bar's in-memory table covers a key range strictly above every table on disk.
- Below level 0, tables overlap nothing in the next level, so compaction uses **move-table**: a manifest update, not a rewrite (`compaction.zig:694-695`, Concept 18 §3.6).
- `lsm.md` predicts this: "Where a tree performs inserts mostly in sort order, with a minimum of updates, this move table optimization should enable the tree's performance to approach that of an append-only log."

**Transfers are never updated in place.** A pending transfer's status (posted, voided, expired) lives in a separate `transfers_pending` groove, keyed by the pending transfer's timestamp (`state_machine.zig:3914-3917`, `:4488-4504`). So the transfers object tree really is append-only.

**Accounts are updated**, because balances change. But the update reuses the same key (`groove.zig:1786-1789`: "the new and old values in the object tree share the same key"). Hot accounts are overwritten many times per bar in the mutable table and de-duplicated before they reach disk.

**Recent data is hot data.** New objects sit in the mutable and immutable tables and the top levels, which are the most likely to be cached. Old transfers sink to the bottom.

**Time-range queries come free.**
- `get_account_transfers` turns `timestamp_min`/`timestamp_max` into a key range over `(debit_account_id, timestamp)` (`state_machine.zig:1812-1831`). That is one contiguous slice per account.
- Grooves can be **joined on timestamp**. `TransferPending` and `AccountEvent` records reuse the transfer's timestamp (`state_machine.zig:3914-3917`, `:3948`). The scan builder relies on this: "Multiple `Groove`s can be queried together in the same scan, as long as related objects share the same `timestamp`" (`scan_builder.zig:31-36`).

**Ids benefit from the same idea.** The `id` tree is keyed by the client's id, so TigerBeetle recommends time-based ids. "TigerBeetle ID" (TBID) puts a 48-bit millisecond timestamp in the high bits and 80 random bits below. Within one millisecond, the client increments rather than re-randomizes (`data-modeling.md`). The docs say random ids "have _significantly_ lower throughput". They give no number, so the magnitude is unverified here.

### 3.6 Imported events: user-supplied timestamps

Some users migrate history from another system and need original timestamps. Setting `flags.imported` lets the client supply `timestamp` itself (`transfer.md`, `account.md`). The rules, as enforced in `execute_create` (`state_machine.zig:3002-3057`) and the create functions:

- **The whole batch must agree.** The first event decides whether the batch is imported. Mixing gives `imported_event_expected` or `imported_event_not_expected` (`:3026-3037`).
- **The timestamp must be valid and in the past.** It must be within `TimestampRange` and `< timestamp_event`, or the event fails with `imported_event_timestamp_out_of_range` or `imported_event_timestamp_must_not_advance` (`:3040-3051`).
- **Non-imported events must send `timestamp = 0`,** or the event fails with `timestamp_must_be_zero` (`:3053-3057`).
- **Accounts must not regress** (`:3624-3640`):
  - the timestamp must be greater than the accounts object tree's `key_range.key_max`;
  - it must not equal any transfer's timestamp.
- **Transfers must not regress** (`:3774-3811`):
  - the timestamp must be greater than the transfers and account-events trees' `key_max`;
  - it must not equal an account's timestamp;
  - it must be later than both accounts' creation timestamps;
  - an imported pending transfer must have `timeout = 0`, because expiry is driven by the cluster clock.

The "must not regress" check is O(1). It reads the tree's in-memory `key_range.key_max`, which only works because the tree is keyed by timestamp. Import still preserves the invariant the LSM wants: **each object tree's keys keep increasing.** Imported history must be loaded in order, and before newer live traffic. That is why the docs recommend importing "only on a fresh cluster or during a scheduled maintenance window".

### 3.7 ⚠️ Docs vs code

The code wins in each case:

- **Upper bound.** `transfer.md` and `account.md` say an imported timestamp must "not be larger than `2^63`". The code's inclusive maximum is `maxInt(u63)` = 2⁶³ − 1 (`timestamp_range.zig:10`), because bit 63 is the tombstone flag.
- **"Globally unique".** `ARCHITECTURE.md` says every object has a globally unique timestamp. That holds for accounts and transfers: the import checks even reject collisions *between* them. But `TransferPending` and `AccountEvent` records deliberately **reuse** the transfer's timestamp as a join key (`state_machine.zig:3914-3917`, `:3948`). Within each groove the timestamp is unique; across grooves it is shared on purpose.
- **"Set when the event arrives".** `time.md` says timestamps are "set by the cluster when the corresponding event arrives at the primary". Precisely, the batch base is `max(previous + 1, synchronized realtime)`, and events are spaced 1 ns apart above it. If the clock lags the previous batch, the timestamp can be later than the true arrival time. It is never earlier than the synchronized clock reading.

## 4. What it means for ashi

**Rust features that help:**
- **A newtype**, `#[repr(transparent)] struct Timestamp(u64)`. Give it `MIN = 1`, `MAX = (1 << 63) - 1` and `TOMBSTONE_BIT = 1 << 63`, with constructors that assert the range. Without it, a bare `u64` timestamp gets mixed up with op numbers and durations.
- **Exhaustive enums for result codes** (`ImportedEventTimestampMustNotRegress`, …) make the import rules hard to forget.
- **`checked_add` / `checked_sub`** around `timestamp - events.len + index + 1`. TigerBeetle asserts `TimestampRange.valid(timestamp_event)`; ashi should too, in release builds.

**Patterns to use:**
- **Inject time; never read it in the state machine.** `fn commit(&mut self, op: u64, timestamp: Timestamp, body: &[u8])`. No `SystemTime::now()` or `chrono::Utc::now()` anywhere below the consensus layer. This one rule makes the state machine replayable (Concept 20) and simulatable (Concept 29).
- **`prepare_timestamp = max(max(prepare, commit) + 1, realtime) + event_count`** in the primary only.
- **Index entries as `(prefix, timestamp)` packed into one integer** (`u128` for a u64 prefix, a `[u64; 3]` or `u256`-like struct for a u128 prefix). Comparisons stay single integer compares, and zig-zag merges work on the timestamp.
- **Ship a TBID-style id generator in ashi's clients,** so the `id` tree gets append-like locality too.

**Crates:**
- `ulid` / `uuid` (v7) give time-ordered ids on the client side, but their layouts differ from TBID. TBID is simple enough (~20 lines) to hand-write, matching TigerBeetle's zero-dependency stance (Concept 13).
- Avoid `chrono` and `time` inside the core. Nanosecond `u64` arithmetic is all the state machine needs. Formatting belongs in tools.

**Pitfalls specific to Rust:**
- `std::time::Instant` is monotonic but has no epoch, so it can't produce a UNIX timestamp. `SystemTime` has an epoch but can go backwards. You need both: a monotonic source for intervals, and the cluster clock for absolute time (Concept 26).
- `SystemTime::duration_since(UNIX_EPOCH)` returns a `Result` and `as_nanos()` returns `u128`. Narrow to `u64` explicitly and assert it fits in 63 bits.
- If ashi's object cache is keyed by `id` but its object tree by timestamp (like TigerBeetle's), keep the invariant "anything in the mutable table is in the cache" (`groove.zig:687-688`). Assert it in debug builds, or lookups will silently miss fresh writes.

**ashi stance:** _to be decided once the drill is done_

Suggestion: **adopt**. One 8-byte, unique, monotonic, wall-clock key gives append-only object trees, cheap index pointers, time-range scans and cross-groove joins, all at once. The cost is a hard dependency on a good cluster clock (Concept 26).

## 5. Drill questions

### Q1. Recall

Who assigns an object's timestamp, at which step, and from what clock? Why must clients send `timestamp = 0` on non-imported events?

> _Your answer:_

### Q2. Mechanism

Walk through `lookup_transfers` for an id that is not in any cache. Name each tree touched, in order, and what each returns. Then explain why the `id` tree stores a timestamp rather than a copy of the transfer.

> _Your answer:_

### Q3. Back-of-envelope

- The previous prepare's timestamp was `T0 = 1,700,000,000,000,000,000`. The primary's synchronized clock now reads `T0 − 5,000` (it lags). A `create_transfers` request arrives with 8,189 transfers. What is the header timestamp, and what are the first and last transfers' timestamps?
- In roughly what calendar year does a u63 nanosecond UNIX timestamp overflow?

> _Your answer:_

### Q4. What breaks if…

A newly elected primary skips the step that raises `prepare_timestamp` to the view-change quorum's maximum, and its clock is 2 seconds behind the old primary. List what goes wrong in the transfers object tree, the secondary indexes and the import checks.

> _Your answer:_

### Q5. ashi design (open-ended)

Should ashi's synthetic key be (a) wall-clock nanoseconds like TigerBeetle, (b) a pure logical counter such as `(op << 16) | index`, or (c) a hybrid logical clock? Consider timeouts, auditing, imports, key width and dependence on clock synchronization.

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- **Who and when:** the **primary**, when it converts a request into a prepare (`primary_pipeline_prepare`).
- **From what clock:** `clock.realtime_synchronized()`, the wall clock clamped to the cluster-agreed window (Concept 26), combined with `max(prepare, commit) + 1` for monotonicity. Then one nanosecond is reserved per event.
- **Each event's timestamp** is `header.timestamp − n + i + 1`.
- **Why clients send 0:** so every timestamp comes from the same cluster clock and is directly comparable, unique and totally ordered (`time.md`, "Why TigerBeetle Manages Timestamps"). A non-zero value on a non-imported event fails with `timestamp_must_be_zero`.

**Q2.**
1. Objects cache keyed by id: miss.
2. The `transfers.id` tree's in-memory key range (`key_range_contains`): the id might exist.
3. Lookup in the `id` unique tree's levels: returns `{id, timestamp}`. Timestamp 0 means orphaned (failed before); a tombstone means not found.
4. Lookup in the transfers **object tree** by that timestamp: returns the 128-byte Transfer, which is put in the objects cache.

Why store a timestamp instead of the transfer:
- the object lives in exactly one tree, so it is written once, and the object tree stays append-only and sorted by time;
- the id tree stays small (32 B per entry, not 128 B);
- every other index can point to the same timestamp.

**Q3.**
- `prepare_timestamp = max(T0 + 1, T0 − 5,000) = T0 + 1`. Adding 8,189 gives a header timestamp of **T0 + 8,190** = 1,700,000,000,000,008,190.
- First transfer: `T − 8,189 + 0 + 1` = **T0 + 2**. Last transfer: **T0 + 8,190**. Timestamps keep rising even though the wall clock lags.
- Overflow: 2⁶³ ns ≈ 9.22 × 10¹⁸ ns ≈ 292 years after 1970, so around **2262**.

**Q4.**
- New timestamps fall **below** existing ones and may **collide** with them.
- **Object tree:** a `put` with an existing timestamp key *overwrites* that object. A new transfer would silently replace an old one.
- **Move-table** stops applying, because new tables overlap old ranges.
- **Secondary indexes:** `(field, timestamp)` pairs could collide or point at the wrong object. Scans would return events out of real-time order.
- **Import checks** that compare against `key_range.key_max` would accept or reject the wrong values.
- **Clients** could see a later transfer with an earlier timestamp, breaking `time.md`'s total-order guarantee.
- Many of these trip assertions first, e.g. `assert(timestamp > self.commit_timestamp)`, crashing the replica. That is the intended outcome under TigerStyle (Concept 30).

**Q5.** There is no single right answer. Points a good answer covers:
- **(a) Wall-clock ns:** one field serves as key, audit time and timeout base, and imports keep real dates. But it requires a cluster clock (Marzullo, Concept 26) and rejects requests when unsynchronized.
- **(b) Logical counter:** no clock dependency for keys, and trivially monotonic. But timeouts and audit still need a separate wall-clock field, which adds bytes and an index. Imports need a mapping from historical time to counter values.
- **(c) HLC:** close to wall time with logical tie-breaking. More complex, and still needs bounded clock skew.
- Width: all three fit in 64 bits for centuries if the tombstone bit is kept.
- For a TigerBeetle-compatible API, (a) matches clients' expectations.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
