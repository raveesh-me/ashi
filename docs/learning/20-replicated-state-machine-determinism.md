# Concept 20: Replicated state machine; identical logical state *and* identical bytes on disk

**Module:** M4 Consensus · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Overview" (lines 27–90), "Determinism" (281–307), "Grid" (397–406), "Time" (489–503)
- `../tigerbeetle/docs/internals/data_file.md`, section on the grid (lines 15–80)
- `../tigerbeetle/docs/internals/sync.md`, section "Storage Determinism"
- `../tigerbeetle/docs/internals/vsr.md`, "Protocol: Repair Grid"
- `../tigerbeetle/docs/TIGER_STYLE.md`, "buffer bleeds" (lines 434–438)
- `../tigerbeetle/src/vsr/replica.zig` (`ReplicaType`, `on_request`, `primary_pipeline_prepare`, `on_prepare`, `execute_op`, `execute_op_reply_to_client`)
- `../tigerbeetle/src/state_machine.zig` (`prepare`, `prefetch`, `commit`, `compact`, `checkpoint`)
- `../tigerbeetle/src/testing/state_machine.zig` (the swap-in "echo" state machine), `../tigerbeetle/src/vopr.zig`
- `../tigerbeetle/src/vsr/superblock.zig` (`CheckpointState`, `checkpoint_id`)
- `../tigerbeetle/src/vsr/free_set.zig` (reservations), `../tigerbeetle/src/lsm/forest.zig` (`compact`)
- `../tigerbeetle/src/stdx/time.zig`, `../tigerbeetle/src/stdx/stdx.zig` (`hash_inline`)
- `../tigerbeetle/src/testing/cluster/storage_checker.zig`
- `../tigerbeetle/src/constants.zig`, `../tigerbeetle/src/config.zig`

## 1. The problem

A ledger runs on several machines (replicas) so it survives a crash. That only helps if every replica holds **the same balances**. If two replicas disagree, a failover silently changes someone's balance. Money appears or vanishes.

There is a second, less obvious need. Disks rot (Concept 14). When one replica finds a corrupt block, it wants a good copy from a peer. That only works if the peer has **the same bytes**, not just the same meaning.

So the question is: how do six machines, with different disks, clocks and scheduling, end up with the same state, down to the byte?

## 2. The obvious solution, and why it fails

**Option A: ship the state.** The primary executes each transfer and sends the changed pages to the backups.

- It is bandwidth-hungry. A TigerBeetle commit message (git `3aa232f95`) estimates that a 128-byte transfer becomes about 1,000 bytes of index data on disk, plus 20–30× LSM write amplification. Shipping pages means shipping all of that. Shipping the *request* means shipping 128 bytes.
- A bug on the primary that corrupts a page is replicated faithfully to everyone.

**Option B: ship the log, let each replica store it however it likes.** Every replica applies the same ordered requests, so balances match (*logical* determinism). But each storage engine compacts when it feels like it, picks its own block addresses, and pads with whatever bytes were in the buffer.

That works until a block goes bad. Back-of-envelope with TigerBeetle's own block size (`block_size = 512 KiB`, `src/config.zig:161`):

```
One flipped byte in a 512 KiB LSM value block on replica A.

Physical determinism:  ask any peer for (address, checksum) → transfer 512 KiB.
Logical determinism:   peer's block at that address holds different bytes.
                       Must rebuild or re-send the whole tree (say 500 GiB).
Ratio: 500 GiB / 512 KiB ≈ 1,000,000×
```

ARCHITECTURE.md makes the same point: logical-only repair "might entail re-transmitting the entire tree. This is slow and might fail in presence of uncorrelated faults."

Option B also hides a trap. In Rust, `HashMap` iteration order differs between processes. If the state machine ever iterates a map to decide something, replicas diverge with no error at all.

## 3. What TigerBeetle does

### 3.1 The recipe: same start, same log, same function

A **replicated state machine (RSM)** is three ingredients:

```
  same start state      same ordered log              same deterministic function
        S0  ──apply(p1)──▶  S1  ──apply(p2)──▶  S2  ── … ──▶  Sn      (on every replica)
```

ARCHITECTURE.md "Overview" (lines 34–52) states it directly:

> Specifically, TigerBeetle is a replicated state machine. The ground state of the system is an immutable, hash-chained, append-only log of prepares. … Because all replicas start with the same (empty) state and the state transition function is deterministic, the replicas arrive at the same state.

Definitions:
- A **prepare** is one log entry: a client batch plus a header that gives it an `op` number, a `timestamp`, and the checksum of the previous prepare (`parent`). Concept 21 covers how the log is agreed.
- The **ground state** is the log. The **derived state** is the LSM forest (balances, transfers, indexes) that applying the log produces (Concept 18).

Consensus (Concepts 21–22) only has to agree on an immutable, hash-chained log. ARCHITECTURE.md calls that "a much simpler problem" than synchronizing mutable state.

### 3.2 Logical vs physical determinism

ARCHITECTURE.md "Determinism" (line 283) defines it:

> Determinism means that given the same input the software gives the same logical result and arrives at it using the same physical path.

- **Logical determinism:** every replica computes the same results: same balances, same reply bytes, same error codes.
- **Physical determinism:** every replica also writes the same bytes to the same grid block addresses. "It is guaranteed that replicas in the cluster converge on a byte-for-byte identical LSM tree structure" (ARCHITECTURE.md line 299).

"Physical" is not "everything on disk". The simulator's storage checker lists exactly what must match (`src/testing/cluster/storage_checker.zig:1-20`):

| Area | Identical across replicas? | Why |
|---|---|---|
| Acquired grid blocks (checked after every compaction bar) | **yes** | the core guarantee |
| Superblock `vsr_state.checkpoint` (checked at checkpoint) | **yes** | it is the root pointer |
| Client replies zone | **yes**, once repair finishes | |
| Superblock headers | no | they hold replica-specific state (e.g. its view) |
| WAL headers | no | faulty slots are deliberately overwritten with corrupt headers |
| WAL prepares | no | a replica can commit and checkpoint an op before writing it to its WAL |
| Unallocated grid blocks | no | free space can hold old garbage, e.g. after state sync |

### 3.3 The checkpoint id: a fingerprint of the physical state

A **checkpoint** is the moment a replica makes its grid state durable by rewriting the superblock (Concept 17). The superblock's `CheckpointState` (`src/vsr/superblock.zig:320`) is a 1,024-byte struct. It holds the addresses and checksums of the manifest blocks, the free-set blocks and the client-session blocks. In other words, it points at the roots of the whole on-disk tree.

```zig
// src/vsr/superblock.zig:444
pub fn checkpoint_id(superblock: *const SuperBlockHeader) u128 {
    return vsr.checksum(std.mem.asBytes(&superblock.vsr_state.checkpoint));
}
```

Because blocks are hash-chained (Concept 15), this one `u128` covers every reachable block. Two replicas with the same `checkpoint_id` have byte-identical reachable grids.

Every prepare carries the `checkpoint_id` it was prepared under (`src/vsr/message_header.zig:717-726`). The header comment explains why: "to strictly bound the number of commits that it may take to discover a divergent replica." A backup checks it on every prepare:

```zig
// src/vsr/replica.zig:2173-2193 (on_prepare, abridged)
if (message.header.checkpoint_id != self.superblock.working.checkpoint_id() and
    message.header.checkpoint_id !=
        self.superblock.working.vsr_state.checkpoint.parent_checkpoint_id)
{
    // If this branch is hit, there is a storage determinism problem. …
    log.err("{f}: on_prepare: checkpoint diverged …", …);
    @panic("checkpoint diverged");
}
```

So divergence is found within one checkpoint interval. With the production config that is `vsr_checkpoint_ops = 1024 - 32 - 32 × ⌈16/32⌉ = 960` ops (`src/constants.zig:40`, with `journal_slot_count = 1024`, `lsm_compaction_ops = 32`, `pipeline_prepare_queue_max = 8` from `src/config.zig:155-164`). `docs/internals/sync.md` "Storage Determinism" adds: the detecting replica panics, and this "should prompt operator investigation".

TigerBeetle treats nondeterminism as a crash-worthy bug, not a condition to tolerate.

### 3.4 Why physical determinism buys block-level repair

A grid block is named by `(address, checksum)`. The checksum lives in the parent, not in the block (Concept 15). Because every up-to-date replica has the same bytes at the same address:

- **Grid repair** (`docs/internals/vsr.md`, "Protocol: Repair Grid"): a replica with a bad block sends `get_blocks` with `address`/`checksum` pairs. Any peer replies with the block, and the receiver checks the checksum it already knows.
- **State sync** (Concept 25): a replica that fell too far behind copies blocks, not rows. `data_file.md` (lines 25–28): "This storage determinism is exploited to implement state sync and repair on the level of grid blocks."
- **Simpler error handling:** "The function that reads data from storage doesn't have an error condition. It _always_ receives the block with the requested checksum but the block can be transparently read from a different replica" (ARCHITECTURE.md lines 301–303).
- **Crash recovery is replay:** after a crash, a replica loads the last checkpoint and replays the WAL suffix. "Determinism guarantees that the replica ends up in the exact same state" (ARCHITECTURE.md lines 74–78).

Concept 17 (copy-on-write grid) and Concept 25 (repair and state sync) build on this.

### 3.5 Every source of nondeterminism, and how it is removed

| Source | TigerBeetle's answer | Where |
|---|---|---|
| Wall-clock time | Primary reads the clock once; the timestamp travels inside the prepare | `replica.zig:1970-1982`, `7354-7364`, `5460-5467` |
| Hash-map iteration order | No per-process random seed; insertion-ordered maps where order matters | `stdx/stdx.zig:579-587`, `lsm/groove.zig:572` |
| Concurrent IO finishing in any order | Free-set *reservations* made in a fixed order | `vsr/free_set.zig:31-44`, `411-436` |
| Compaction timing | Compaction is paced by op number, not by idle time | `lsm/forest.zig:384-391` |
| Threads | There are none on the replica (Concept 6) | `ARCHITECTURE.md` "Single Thread" |
| Randomness | Explicit `stdx.PRNG`, seeded; state-affecting choices use shared seeds | `replica.zig:1450`, `5360` |
| Uninitialized bytes | No implicit padding; sector tails asserted zero | `message_header.zig:71-74`, `replica.zig:7441` |
| Configuration | Cluster config is fingerprinted; all replicas must match | `config.zig:145-149`, `198` |
| Code version | A checkpoint records which release must execute its ops | `superblock.zig:384` (Concept 32) |

**Time.** The state machine never reads a clock. The primary reads the cluster-synchronized clock once per request (`on_request`, `src/vsr/replica.zig:1970-1982`). It refuses the request if the clock isn't synchronized (Concept 26). Then it computes a strictly increasing timestamp:

```zig
// src/vsr/replica.zig:7354-7364 (primary_pipeline_prepare)
// Guard against the wall clock going backwards by taking the max with timestamps issued:
self.state_machine.prepare_timestamp = @max(
    @max(self.state_machine.prepare_timestamp, self.state_machine.commit_timestamp) + 1,
    @as(u64, @intCast(request.realtime)),
);
```

`StateMachine.prepare` then adds one nanosecond per event in the batch (`src/state_machine.zig:1146-1176`). The result goes into the prepare header's `timestamp` field (`message_header.zig:733-735`). Every replica, primary or backup, calls `commit` with `prepare.header.timestamp` (`replica.zig:5460-5467`). Inside `commit`, each event gets `timestamp - events.len + index + 1` (`state_machine.zig:3005`). Those timestamps become primary keys (Concept 19), so they *must* be identical.

Even the clock itself is an interface: `stdx.Time` is a vtable with `monotonic` and `realtime` (`src/stdx/time.zig:15-37`), backed by `TimeOS` in production and `TimeSim` in the simulator (`time.zig:40`, `143`).

**Hash maps.** TigerBeetle does use hash maps, for example the object cache (`src/lsm/cache_map.zig:47-58`). Its hash function passes an explicit seed of zero:

```zig
// src/stdx/stdx.zig:579-587
pub inline fn hash_inline(value: anytype) u64 {
    comptime {
        assert(no_padding(@TypeOf(value)));
        assert(has_unique_representation(@TypeOf(value)));
    }
    return low_level_hash(0, switch (@typeInfo(@TypeOf(value))) { … });
}
```

Where iteration order matters, the code uses `ArrayHashMap`, which iterates in insertion order (e.g. the prefetch key set, `src/lsm/groove.zig:572`). Note the `has_unique_representation` assert: it forbids hashing types where equal values could have different bytes.

> ⚠️ **Not verified here:** Zig's standard-library hash maps (`std.AutoHashMap`) use an unseeded hash, unlike Rust's `RandomState`. That is a property of Zig's stdlib, which is not in the clone. I also did not audit every map iteration in the codebase.

**IO completion order.** Prefetch and compaction issue many reads at once through io_uring (Concept 12). They finish in whatever order the disk likes. If blocks were allocated "first come, first served", two replicas would place the same table at different addresses. The free set prevents this:

```zig
// src/vsr/free_set.zig:31-44 (doc comment, abridged)
/// Concurrent callers must reserve free blocks before acquiring them to ensure that
/// acquisition order is deterministic despite concurrent jobs acquiring blocks in
/// nondeterministic order.
///   1. Reserve: In deterministic order, each job (e.g. compaction) calls `reserve()` …
///   2. Acquire: The jobs run concurrently. Each job acquires blocks only from its
///      respective reservation (via `acquire()`).
```

Inside a reservation, `acquire` takes the lowest free bit (`free_set.zig:411-436`).

**Compaction scheduling.** An LSM engine usually compacts when it is idle or a level is full. That depends on timing. TigerBeetle compacts a fixed slice of work after every commit, chosen by op number:

```zig
// src/lsm/forest.zig:384-391
pub fn compact(forest: *Forest, callback: Callback, op: u64) void {
    const compaction_beat = op % constants.lsm_compaction_ops;
    const first_beat = compaction_beat == 0;
    …
```

A **beat** is one op's share of compaction. A **bar** is `lsm_compaction_ops = 32` beats (Concept 18). Even the memory budget is a local knob that does not change the result. From `Forest.Options` (`forest.zig:173-176`): "Compactions will be deterministic regardless of how much blocks you give them, but will run in fewer steps with more memory."

**Randomness.** There is no global RNG. Two kinds of seeded `stdx.PRNG` exist:
- *Replica-local*, seeded from `random_nonce` (`replica.zig:1450`). In production the nonce comes from `io.crypto_random_int` (`src/tigerbeetle/main.zig:366`). In the simulator it comes from the seed (`src/testing/cluster.zig:454`). From the call sites I found, it is used only for control-plane choices: timeout jitter (`replica.zig:3685`, `3795`), which peer to ask for repair (`3556`, `4300`), and injected commit stalls (`4852`). None of these feed the state machine or block layout.
- *Cluster-shared*, seeded from data every replica knows. For each op, one backup also replies to the client. They agree on which one without a message:

```zig
// src/vsr/replica.zig:5360-5364 (execute_op_reply_to_client)
var prng = stdx.PRNG.from_seed(op);
const offset_random = prng.range_inclusive(u8, 1, self.replica_count - 1);
const backup_random = (self.primary_index(self.view) + offset_random) % self.replica_count;
```

**Uninitialized bytes.** A struct with compiler-inserted padding, or a buffer only partly overwritten, puts leftover memory on disk. TIGER_STYLE calls this a **buffer bleed** and notes it "may cause deterministic guarantees as required by TigerBeetle to be violated" (lines 434–438). Defences:
- `assert(stdx.no_padding(Header))` and the same for `CheckpointState` (`message_header.zig:73`, `superblock.zig:395`).
- When the primary builds a prepare it asserts the sector tail is zero: `assert(stdx.zeroed(message.buffer[message.header.size..size_ceil]))` (`replica.zig:7441`).

**Configuration and code version.** `ConfigCluster` "must have the same configuration" on all replicas, and "storage formats generated by different ConfigClusters are incompatible" (`config.zig:145-149`). Its checksum is a cluster fingerprint (`config.zig:198`). The checkpoint also records the release that must execute its ops (`superblock.zig:384`). A different binary could compute a different answer, so upgrades are coordinated (Concept 32).

### 3.6 The swappable state machine

The replica is generic over the state machine:

```zig
// src/vsr/replica.zig:144-149
pub fn ReplicaType(
    comptime StateMachine: type,
    comptime MessageBus: type,
    comptime Storage: type,
    comptime AOF: type,
) type {
```

ARCHITECTURE.md (lines 23–25): "TigerBeetle implements accounting logic but the underlying state machine can be swapped."

The contract, as the replica calls it, per op:

```
 primary only            every replica, in op order
 ────────────            ──────────────────────────────────────────────────────────────
 prepare(op, body)  ──▶  prefetch(cb, op, snapshot, op, body)   async: load needed rows
 (adds to                commit(client, op, timestamp, operation, body, out) -> usize
  prepare_timestamp)                                              sync: pure CPU, no IO
                         compact(cb, op)                         async: one beat of LSM work
                         checkpoint(cb)                          async: every vsr_checkpoint_ops
```

Other required parts: an `Operation` enum (values ≥ `vsr_operations_reserved = 128`, `constants.zig:31`), a `Forest` type, `init`/`deinit`/`reset`/`open`, `input_valid`, `pulse_needed`, and the `prepare_timestamp`/`commit_timestamp` fields the replica reads.

Note what `commit` receives: `client`, `op`, `timestamp`, `operation` and the body. All of it comes from the prepare. There's no replica index, no wall clock, no client version. `state_machine.zig:2547` says so explicitly: "This function should never accept `client_release` as an argument."

A second implementation exists for testing: `src/testing/state_machine.zig`. It has a single `echo` operation that stores a checksum of its inputs in a one-groove forest. The simulator picks between them at build time:

```zig
// src/vopr.zig:25-29
const StateMachineType = switch (state_machine) {
    .accounting => @import("state_machine.zig").StateMachineType,
    .testing => @import("testing/state_machine.zig").StateMachineType,
};
```

The consensus, storage and repair code is tested without accounting logic in the way.

## 4. What it means for ashi

**Make the transition function a pure function of the prepare.** Signature sketch:

```rust
pub trait StateMachine {
    type Operation: Copy + Into<u8>;
    fn prepare(&mut self, op: Self::Operation, body: &[u8]);          // primary only
    fn prefetch(&mut self, op_number: u64, op: Self::Operation, body: &[u8]); // starts IO
    fn commit(&mut self, client: u128, op_number: u64, timestamp: u64,
              op: Self::Operation, body: &[u8], out: &mut [u8; MESSAGE_BODY_MAX]) -> usize;
    fn compact(&mut self, op_number: u64);
    fn checkpoint(&mut self);
}
```

Use static dispatch (`Replica<S: StateMachine, B: Bus, St: Storage>`), the way Zig uses `comptime`. `Box<dyn StateMachine>` buys nothing and costs inlining.

**Rust features that help:**
- Traits plus generics give the swappable state machine for free, with monomorphization like Zig's.
- `#[repr(C)]` plus `zerocopy`/`bytemuck` derives can prove "no padding" at compile time, the equivalent of `stdx.no_padding`.
- `BTreeMap` iterates in key order. It is a safe default wherever order matters.
- `#![forbid(unsafe_code)]` in the state-machine crate stops a whole class of uninitialized-memory bugs.

**Rust pitfalls specific to determinism:**
- **`HashMap`'s default `RandomState`** seeds SipHash per process. Iteration order differs between replicas and between runs. Use `BuildHasherDefault<FxHasher>` (`rustc-hash`) or a fixed-seed hasher, `BTreeMap`, or `IndexMap` for insertion order. Consider a clippy `disallowed-types` rule banning `std::collections::HashMap` in the core crates.
- **Floats.** Rust's float *formatting* is deterministic. The real risks are float *math*: libm functions like `exp` or `sin` are not guaranteed to round the same on every platform, `mul_add` vs separate ops, and NaN payload bits. A ledger should use integers only. TigerBeetle uses `u64`/`u128` amounts.
- **`rayon` and other thread pools.** Work-stealing order is nondeterministic. Any reduction that is not associative, or any side effect in a `par_iter`, can differ between replicas. Keep the state machine single-threaded (Concept 6).
- **Async runtimes.** `tokio::select!` picks a ready branch at random unless you write `biased;`. Task wake order is not deterministic either. Neither belongs in the state transition.
- **Allocator-dependent behaviour.** Anything keyed or sorted by a pointer address (`*const T as usize`, `Rc::as_ptr`, `Arc::ptr_eq` ordering) changes run to run. Avoid it in anything that reaches disk.
- **Uninitialized bytes.** Writing a `#[repr(C)]` struct with implicit padding to disk via `unsafe` casting leaks old memory: a buffer bleed. `MaybeUninit` buffers sent to disk must be fully written. Assert sector tails are zero, as TigerBeetle does.
- **`Instant::now()` / `SystemTime::now()`** anywhere under `commit`. Inject time through a trait (`trait Clock { fn monotonic(&self) -> u64; fn realtime(&self) -> i64; }`) and pass only the prepare's timestamp into the state machine.
- **Iterating the filesystem** (`read_dir` order), environment variables, or `std::thread::available_parallelism()` in anything that shapes state.

**Patterns to adopt:**
- A `checkpoint_id` equivalent: a checksum of the root struct, carried in every prepare, with a panic on mismatch. It is cheap and catches the bugs above within one checkpoint.
- A simulator storage checker that compares acquired blocks byte for byte across replicas after every compaction bar (Concept 29).
- A test-only state machine (an "echo"), so consensus tests don't depend on ledger logic.

**Crates worth evaluating** (TigerBeetle has zero dependencies; Concept 13):
- `rustc-hash` or `foldhash` (has a fixed-seed mode) for deterministic hashing. Both are tiny.
- `indexmap` for insertion-ordered maps.
- `zerocopy` for padding-free layouts.
- `rand_chacha` / `rand_xoshiro` for explicit, seedable PRNGs. Never use `thread_rng()` in the core.

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt**. Physical determinism is what makes block-level repair, state sync and simulation testing possible. Retrofitting it later is nearly impossible.

## 5. Drill questions

### Q1. Recall

Name the four ways TigerBeetle stops wall-clock time from making replicas diverge. Trace one transfer's timestamp from the primary's clock to the `timestamp` field of the stored `Transfer`.

> _Your answer:_

### Q2. Mechanism

Explain why the free set needs *reservations*. What would go wrong if compaction jobs simply called `acquire()` whenever they needed a block, given that io_uring completes reads in any order?

> _Your answer:_

### Q3. Back-of-envelope

With the production config, how many ops can a replica commit after its storage silently diverges before another replica's prepare triggers `checkpoint diverged`? Show the `vsr_checkpoint_ops` calculation. Then: a 1 TiB data file has one corrupt 512 KiB block. How much data crosses the network to repair it under physical determinism, versus repairing by re-sending the whole grid?

> _Your answer:_

### Q4. What breaks if…

…a developer adds a performance tweak: "if the disk is idle, run the next compaction beat early, up to 4 beats ahead." Balances stay correct. What breaks, and how would TigerBeetle find out?

> _Your answer:_

### Q5. ashi design (open-ended)

ashi wants a pluggable state machine, and it wants to let operators tune memory (cache sizes, compaction memory) per replica. Which knobs can differ between replicas without breaking physical determinism, and which must be cluster-wide? How would ashi enforce the cluster-wide ones?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- (1) Only the primary reads the clock, once per request, in `on_request` (`clock.realtime_synchronized()`; it drops the request if the clock isn't synchronized).
- (2) It takes `max(prev + 1, realtime)`, so time is strictly monotonic even if the wall clock jumps back.
- (3) The result travels inside the prepare header (`timestamp`), so backups never consult their own clock.
- (4) The state machine has no clock access at all. `commit` receives `prepare.header.timestamp` as an argument, and the clock is an injected `Time` vtable (`TimeSim` in tests).
- Trace: `realtime` → `prepare_timestamp = max(…)` → `StateMachine.prepare` adds the batch's event count → header `timestamp` = the batch's highest timestamp → `commit(…, timestamp, …)` on every replica → event `i` of `n` gets `timestamp - n + i + 1` → stored as `Transfer.timestamp`, which is also its key (Concept 19).

**Q2.**
- Several compaction jobs run at once, and their reads finish in disk order, which differs per replica.
- If each job grabbed the lowest free block when it got there, replica A might give block 1000 to tree X and 1001 to tree Y, while replica B does the opposite.
- The logical content is the same, but the addresses differ. So the manifests differ, `checkpoint_id` differs, and the next prepare panics with `checkpoint diverged`. Grid repair by `(address, checksum)` would also be impossible.
- Reservations are made in a fixed order *before* the concurrent phase. Each job then only acquires from its own range, so timing can't change which addresses it gets.

**Q3.**
- `vsr_checkpoint_ops = journal_slot_count − lsm_compaction_ops − lsm_compaction_ops × ⌈2·pipeline_prepare_queue_max / lsm_compaction_ops⌉ = 1024 − 32 − 32 × ⌈16/32⌉ = 1024 − 32 − 32 = 960`.
- Divergence changes the next `checkpoint_id`. Prepares carry the checkpoint id they were prepared under, so a mismatch surfaces at the latest on the first op after the next checkpoint: on the order of one checkpoint interval, about 960 ops. The header comment: "discovered *at latest* when the divergent replica attempts to commit the first op after the next checkpoint."
- Repair: 512 KiB (plus a small `get_blocks` request) versus up to ~1 TiB. That is a factor of 2,097,152 (1 TiB / 512 KiB = 2²⁰ × 2 = 2,097,152).

**Q4.**
- Compaction would then depend on disk timing, which differs per replica. Tables get merged at different points, new blocks get allocated at different addresses, and the manifest log differs.
- Balances (logical state) still match, but the grid bytes don't. Physical determinism is gone. Grid repair and state sync would fetch blocks that don't fit the requester's tree.
- Detection: at the next checkpoint, replicas compute different `checkpoint_id`s. The first prepare carrying the primary's id makes a backup panic in `on_prepare` with "checkpoint diverged". In the simulator, `StorageChecker` would fail even earlier, at the end of the compaction bar.

**Q5.** Open-ended. Points a good answer weighs:
- **Can differ** (they affect speed, not bytes): cache sizes, the compaction memory budget (TigerBeetle says compaction is deterministic "regardless of how much blocks you give them"), IO depth, timeouts, log level, PRNG seed for jitter.
- **Must match:** block size, WAL slot count, pipeline depth, `lsm_compaction_ops`, LSM levels and growth factor, batch/message size, `clients_max` (it sizes the client-replies zone), the state machine's code version, and the hash function used for anything persisted.
- **Enforcement:** fingerprint the cluster config (as `ConfigCluster::checksum` does) and put it in every handshake and in the superblock; refuse to join on mismatch. Put the release in the checkpoint. Carry a `checkpoint_id` in prepares and panic on mismatch.
- **Pluggable state machines:** require the trait's `commit` to take only prepare-derived inputs. Run a simulator storage checker against every state machine before shipping it.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
