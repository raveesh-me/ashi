# Concept 31: Fuzzing each component

**Module:** M5 Correctness · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/src/fuzz_tests.zig` (the entry point: `Fuzzers`, `main`, `main_smoke`, `main_single`)
- `../tigerbeetle/src/stdx/testing/fuzz.zig` (helpers: `FuzzArgs`, `random_enum_weights`, `random_id`, `parse_seed`, `limit_ram`)
- Every `*_fuzz.zig` registered in `fuzz_tests.zig` (listed in 3.2), in particular `src/vsr/free_set_fuzz.zig`, `src/lsm/forest_fuzz.zig`, `src/lsm/tree_fuzz.zig`, `src/storage_fuzz.zig`, `src/message_bus_fuzz.zig`, `src/state_machine_fuzz.zig`
- `../tigerbeetle/src/lsm/segmented_array.zig` (`FuzzContextType`, `run_fuzz`)
- `../tigerbeetle/docs/internals/HACKING.md` (fuzzing commands)
- `../tigerbeetle/docs/TIGER_STYLE.md`, section "Safety" (assertions as a "force multiplier for discovering bugs by fuzzing")
- `../tigerbeetle/build.zig` (`build_fuzz`, CI step `fuzz -- smoke`)
- `../tigerbeetle/src/scripts/devhub.zig` (`devhub_coverage`)
- `../tigerbeetle/CHANGELOG.md` (CFO history), and the last in-repo version of `src/scripts/cfo.zig` (via `git show 34a6d6bde^:src/scripts/cfo.zig`, read-only)

## 1. The problem

The VOPR (Concept 29) tests the whole cluster. That is powerful, but whole-system testing has blind spots.

- **Depth.** To reach a rare state inside one data structure, the VOPR has to pass through clients, consensus, the state machine and the LSM. A bug in the free set that needs 50,000 specific acquire/release steps may never be reached within a 96-request VOPR run.
- **Speed.** One VOPR seed simulates a full cluster with up to 12 nodes. A free-set fuzzer can run millions of operations per second on one small structure.
- **Mocked layers.** The VOPR swaps out the production `StorageType(IO)` and `MessageBusType(IO)` (Concept 29, 3.2). Bugs in those layers are invisible to it by design.
- **Debuggability.** When the VOPR fails, the root cause may be ten layers away from the assert that fired. A component fuzzer fails next to the bug.

For a ledger, every one of these components sits on the path to money being right. A free-set bug means a block gets reused while still referenced, which corrupts the grid. An EWAH codec bug means a free set decodes wrong after restart. These need direct, deep testing.

## 2. The obvious solution, and why it fails

### Option A: example-based unit tests

```zig
test "free set acquire then release" {
    var fs = try FreeSet.open_empty(gpa, 64);
    const r = fs.reserve(1).?;
    const a = fs.acquire(r).?;
    fs.release(a);
    ...
}
```

- You test the cases **you thought of**. Bugs live in the cases you didn't.
- The interesting state space is combinatorial. Reservations interleaved with checkpoints, forfeits and releases across shard boundaries number in the millions of distinct sequences.

### Option B: classic coverage-guided byte fuzzing (AFL, libFuzzer)

Feed random bytes, keep inputs that reach new branches.

- Good at parsers, where input *is* bytes.
- Poor at **stateful APIs**. Most random byte strings decode to garbage call sequences that fail validation early. Hitting "reserve 3 blocks, acquire twice, checkpoint, release the first" needs structure.
- It finds **crashes**, but not **wrong answers**. A free set that returns a *valid-looking but wrong* address won't crash. You need an **oracle** that knows the right answer.

### What you actually want

1. **Structured generation.** Generate *operations* (enum-tagged events), not bytes.
2. **An oracle.** Compare every result against a slow, obviously correct **reference model**.
3. **Seeded determinism.** One `u64` reproduces the run, exactly as with the VOPR.
4. **Swarm configuration.** Randomize the operation *mix* per seed too, so some seeds stress rare operations.

That is **model-based (differential) fuzzing**.

## 3. What TigerBeetle does

### 3.1 The entry point: `src/fuzz_tests.zig`

There is one binary, `fuzz`, built from `src/fuzz_tests.zig` (`build.zig:1337-1368`). It is compiled with the `test_min` config (`fuzz_tests.zig:20`), the same tiny-sizes config the VOPR uses, so edge cases come quickly. It requires `constants.verify` (`fuzz_tests.zig:69`), so extra expensive internal checks are on.

The registry is a compile-time struct (`fuzz_tests.zig:35-56`):

```zig
const Fuzzers = .{
    .ewah = @import("./ewah_fuzz.zig"),
    .lsm_scan = @import("./lsm/scan_fuzz.zig"),
    ...
    .state_machine = @import("./state_machine_fuzz.zig"),
    // A fuzzer that intentionally fails, to test fuzzing infrastructure itself
    .canary = {},
    // Quickly run all fuzzers as a smoke test
    .smoke = {},
};
```

Every fuzzer module exposes the same contract:

```zig
pub fn main(gpa: std.mem.Allocator, args: fuzz.FuzzArgs) !void
// FuzzArgs = struct { seed: u64, events_max: ?usize }   (stdx/testing/fuzz.zig:85-88)
```

That uniform shape is what lets a fleet scheduler treat every fuzzer as "a name plus a seed". The CLI (`fuzz_tests.zig:60-66`) is `fuzz [--events-max=N] <fuzzer> [seed]`:

```
./zig/zig build fuzz -- smoke          # all fuzzers, seed 123, small budgets (CI)
./zig/zig build fuzz -- lsm_tree       # random seed from OS entropy, printed
./zig/zig build fuzz -- lsm_tree 92    # replay seed 92
```

Two special entries:

- **`canary`** (`fuzz_tests.zig:151-155`) exits with status 1 when `seed % 100 == 0`. It is a fuzzer that is *supposed* to fail about 1% of the time. If the fleet ever stops reporting canary failures, the failure-reporting pipeline is broken. This is "test the tester", the same idea as checking that a smoke detector beeps.
- **`smoke`** (`fuzz_tests.zig:91-134`) runs every fuzzer once with seed 123 and a small `events_max` (for example `lsm_forest => 10_000`, `vsr_superblock => 3`). It logs an error if any single fuzzer takes over 10 s. CI runs it on every change: `build_ci_step(b, step_ci, .{ "fuzz", "--", "smoke" }, ...)` (`build.zig:646`).

`fuzz.limit_ram()` (`stdx/testing/fuzz.zig:131-140`) caps the address space at 20 GiB on Linux. A runaway fuzzer then fails fast instead of taking down a shared fuzzing machine.

### 3.2 The fuzzers

All 16 real fuzzers registered in `fuzz_tests.zig:36-51`, with what they compare against:

| Name | File | Component | Oracle / strategy |
|---|---|---|---|
| `ewah` | `src/ewah_fuzz.zig` (171 lines) | EWAH bitset compression (used to persist the free set) | **Round-trip:** `decode(encode(x)) == x`, for word types u8…u64 and usize, with random chunk sizes |
| `lsm_scan` | `src/lsm/scan_fuzz.zig` | LSM query engine: arbitrary `(a OR b) AND (c OR …)` index scans | **Model:** a plain ordered `ArrayList(Thing)` plus per-query match bitsets (line 526) |
| `lsm_cache_map` | `src/lsm/cache_map_fuzz.zig` | CacheMap (cache plus stash with scopes and undo) | **Model:** `AutoHashMap` plus an undo log (line 212) |
| `lsm_forest` | `src/lsm/forest_fuzz.zig` | the whole Forest (all grooves, secondary indexes, checkpoint, crash recovery) | **Model with crash semantics** (3.4) |
| `lsm_manifest_log` | `src/lsm/manifest_log_fuzz.zig` | ManifestLog open/insert/update/remove/compact/checkpoint | **Model:** `ManifestLogModel` (line 565); checks recovery equals last checkpoint |
| `lsm_manifest_level` | `src/lsm/manifest_level_fuzz.zig` | ManifestLevel | **Model:** "a separate table buffer to ensure the tables in both match" (lines 5-7) |
| `lsm_segmented_array` | `src/lsm/segmented_array_fuzz.zig` → `segmented_array.zig:1426` | SegmentedArray | **Model:** `reference: std.ArrayList(T)` (`segmented_array.zig:1088`) |
| `lsm_tree` | `src/lsm/tree_fuzz.zig` | one LSM tree: put/remove/get/scan/compact | **Model:** a `SortedSegmentedArray` (line 684), which is itself fuzzed above |
| `message_bus` | `src/message_bus_fuzz.zig` | **production** `MessageBusType(IO)` over a fake socket IO | **Property:** every message is eventually delivered exactly to its intended peer |
| `storage` | `src/storage_fuzz.zig` | **production** `StorageType(IOTest)` read path with latent sector errors | **Property:** stored == written, read == stored, with faulty sectors zeroed |
| `vsr_free_set` | `src/vsr/free_set_fuzz.zig` | FreeSet reserve/acquire/release/checkpoint | **Model:** `FreeSetModel` built on three plain bitsets (line 160) |
| `vsr_superblock` | `src/vsr/superblock_fuzz.zig` | SuperBlock open/checkpoint/view_change, with crashes and disk faults | **Invariants:** quorum always found, never regresses, no state leaks before durability (lines 1-11) |
| `vsr_superblock_quorums` | `src/vsr/superblock_quorums_fuzz.zig` | superblock quorum selection | `main` is **empty** (see the ⚠️ note below); the real fuzzing is in `test "Quorums: fuzz …"` unit tests |
| `vsr_multi_batch` | `src/vsr/multi_batch_fuzz.zig` | multi-batch encoder/decoder (several batches in one message) | **Round-trip** with element sizes 2⁰…2⁸ bytes, body sizes from `sector_size - @sizeOf(Header)` to `1 MiB - @sizeOf(Header)` |
| `signal` | `src/clients/c/tb_client/signal_fuzz.zig` | tb_client cross-thread `Signal` | **Property** under real `std.Thread.spawn` (line 47), up to 8 threads |
| `state_machine` | `src/state_machine_fuzz.zig` | the accounting StateMachine, single node | **Crash-freedom** plus **status coverage** (3.5) |

> ⚠️ **Docs vs code.**
> - `superblock_quorums_fuzz.zig:16-18` has `pub fn main(...) !void { // TODO: remove one CFO is updated. }`. It is registered as a fuzzer, but running it does nothing. Its randomized coverage comes from `zig build test`, seeded by `std.testing.random_seed`.
> - `state_machine_fuzz.zig:1-2` says it looks for ops that "cause a crash, then be replayed after said crash, resulting in a crash loop". But line 199 says "This fuzzer never reopens storage". It does not replay after a crash. It hunts for any *valid* input that trips an assertion. Replay is implied: in a deterministic replicated state machine (Concept 20), an op that crashes once crashes every replica on every replay. So finding the crash is enough.

### 3.3 Anatomy of a model-based fuzzer: `vsr_free_set`

The free set tracks which grid blocks are in use (Concept 17). The production `FreeSet` is optimized: sharded bitsets with a shard index, and released blocks freed only once the next checkpoint is durable. The model is deliberately dumb (`free_set_fuzz.zig:160-171`):

```zig
const FreeSetModel = struct {
    /// Set bits indicate acquired blocks.
    blocks_acquired: std.DynamicBitSetUnmanaged,
    /// Set bits indicate blocks that will be released at the next checkpoint.
    blocks_released: std.DynamicBitSetUnmanaged,
    /// Set bits indicate blocks that are currently reserved and not yet forfeited.
    blocks_reserved: std.DynamicBitSetUnmanaged,
    ...
```

The run loop applies every event to **both** and compares (`free_set_fuzz.zig:47-108`):

```zig
.reserve => |reserve| {
    const reservation_actual = free_set.reserve(reserve.blocks);
    const reservation_expect = free_set_model.reserve(reserve.blocks);
    assert(std.meta.eql(reservation_expect, reservation_actual));
    ...
},
...
// after every event:
assert(free_set_model.count_free() == free_set.count_free());
assert(free_set_model.count_acquired() == free_set.count_acquired());
```

The shape, which every model-based fuzzer shares:

```
            seed
             │
         ┌───▼────┐   swarm: weights per event kind drawn from the seed
         │  PRNG  │   (.reserve = prng.range_inclusive(u64, 1, 100), ...)
         └───┬────┘
             │  events: [reserve, acquire, acquire, release, checkpoint, ...]
     ┌───────┴────────┐
     ▼                ▼
 ┌────────┐      ┌──────────┐
 │  real  │      │  model   │   slow, obviously correct, no optimizations
 │FreeSet │      │ 3 bitsets│
 └───┬────┘      └────┬─────┘
     │   result_a     │ result_b
     └──────► assert(eql) ◄────┘   + assert on aggregate state after every step
```

Details worth copying:

- **Event count is exponential:** `fuzz.random_int_exponential(&prng, usize, blocks_count * 100)` (`free_set_fuzz.zig:18`). Most runs are short and fast. A few are very long and reach deep states.
- **Size is random too:** `blocks_count = FreeSet.shard_bits * prng.range_inclusive(usize, 1, 10)`. Shard boundaries move between seeds.
- **The model must not share code with the implementation.** The forest fuzzer says so explicitly: "The goal is to keep the model independent of the LSM / forest implementation." (`forest_fuzz.zig:496`). If the model reuses the real code, a shared bug passes both sides.

### 3.4 Models with crash semantics: `lsm_forest`

Durability bugs only show up across a crash. The forest fuzzer's operations (`forest_fuzz.zig:30-53`) carry a **modifier**:

```zig
const FuzzOpModifier = union(enum) {
    normal,
    crash_after_ticks: usize,
};
```

It runs on the **testing** `Storage` (`forest_fuzz.zig:16`), the same in-memory faulty disk the VOPR uses, so it can cut power mid-write. The model must then know what *should* survive (`forest_fuzz.zig:484-497`):

```
// The model maintains two sets of transfers and a log:
// 1. transfers_mutable: contains all transfers in their current state.
// 2. transfers_stashed: contains transfers as of the latest checkpoint.
// 3. log: records updates not yet covered by a checkpoint ...
// On storage reset, transfers_mutable is restored from transfers_stashed and the log is discarded.
```

```
 ops:      put put rm  [checkpoint]  put put  ✗crash   reopen → get(...)
 model:    log prefix (≤ checkpointable op) ──► stash;   on crash: mutable := clone(stash), log := ∅
 forest:   memtable/compaction/manifest/superblock ...    recovers from last checkpoint
 check:    every get/scan after reopen must equal model.transfers_mutable
```

This is exactly the checkpoint contract of Concepts 16-18: after a crash, state equals the last durable checkpoint, and the WAL replays the rest. In the full system the WAL replay is the replica's job. The forest fuzzer tests the layer below in isolation, so its model throws away the log.

### 3.5 Fuzzing the fuzzer: coverage tests

A generator can silently stop producing interesting inputs. For example, a refactor might make every transfer fail `input_valid`. TigerBeetle defends against that in two ways.

1. **Coverage assertions in code.** `state_machine_fuzz.zig:929` (`test "options swarm coverage"`) replays nine hand-picked seeds and **fails if any result status is never produced**:

   ```zig
   const seeds = [_]u64{
       34, // Broad account and transfer validation coverage.
       14, // Imported timeout, pending amounts, posting, and expiration.
       158, // Already voided, pending debit overflow, and duplicate user_data_128.
       ...
   };
   ...
   if (!transfers_covered.get(status)) { std.debug.print("uncovered transfer status: {s}\n", ...); complete = false; }
   ```

   Every `CreateTransferStatus` (except deprecated ones) must be reachable by the generator. That makes the result enum a coverage checklist.

2. **Line coverage published on DevHub.** `devhub_coverage` (`scripts/devhub.zig:58-99`) runs `kcov` over the unit tests, `fuzz --events-max=500000 lsm_tree 92`, `fuzz ... lsm_forest 92` and `vopr 92`, and publishes the HTML report. Humans look at it. There is **no** compiler-instrumented coverage feedback loop. `build_fuzz` (`build.zig:1337-1368`) builds a plain executable with no libFuzzer or sanitizer-coverage flags.

Other helpers that make generators better (`stdx/testing/fuzz.zig`):

- `random_enum_weights` (line 34): swarm testing. "some variants are disabled completely, and the rest have wildly different probabilities."
- `random_id` (line 62): flips a coin between a small "hot" ID range (to force collisions and duplicates) and a large "cold" range (to overflow caches). The comment names the tension: "We want to cause many collisions" vs "generate enough ids that various caches can't hold them all."
- `int_edge_biased` in `state_machine_fuzz.zig:467` biases integers toward bit edges (`0`, `maxInt(u63)`, `maxInt(u64)` and so on). That is where overflow checks live. Ledgers are full of u128 overflow checks (Concept 2).

### 3.6 Fuzzing the layers the VOPR mocks

Two fuzzers deliberately test **production IO code** that the VOPR replaces.

- **`storage`** (`storage_fuzz.zig`) instantiates the *real* `StorageType(IOTest)` (line 8) over a fake file with a `fault_map` of bad sectors. The real storage layer has a recovery path. On `error.InputOutput` from a large read it "subdivides" the read in a binary search for the bad sector, then zeroes the unreadable sector (`src/storage.zig:255-305`), and checksums upstream then detect it (Concept 14). The fuzzer checks that after writes and shuffled reads of random lengths, the data read equals the data stored, with faulty sectors zeroed.
- **`message_bus`** (`message_bus_fuzz.zig`) runs the *real* `MessageBusType(IO)` (line 20) over a fake socket `IO` (line 406). That fake injects partial sends and receives, receive errors, send corruption, operations after shutdown, and accept/close errors (lines 85-100, each probability swarm-drawn). It also sends "misdirected" pings to test peer-identity handling. The property: every non-ping message is delivered to its intended destination. A comment notes a portability limit: "fuzzer seeds are only reproducible on the same OS" (lines 422-424), because error sets differ per OS.

So the testing pyramid has no holes:

```
        ┌─────────────────────────────┐
        │ Vortex: real processes,     │  real io_uring/TCP, client drivers (non-deterministic)
        │ real binary (Concept 29)    │
        ├─────────────────────────────┤
        │ VOPR: whole cluster, fake   │  consensus, recovery, repair, sync, upgrades
        │ IO, deterministic           │
        ├─────────────────────────────┤
        │ component fuzzers           │  forest, tree, scan, manifest, free set, superblock,
        │ (model-based, seeded)       │  ewah, multi-batch, state machine,
        │                             │  + REAL storage & REAL message bus over fake IO
        ├─────────────────────────────┤
        │ unit + snapshot tests       │  zig build test
        └─────────────────────────────┘
```

One component is **not deterministic**: `signal` spawns real OS threads (`signal_fuzz.zig:47`), because `Signal` is the one place in the client where threads meet. A seed fixes the plan, not the interleaving.

### 3.7 Running fuzzers continuously: the CFO

The **CFO (Continuous Fuzzing Orchestrator)** is the fleet that runs fuzzers and VOPR seeds around the clock.

> ⚠️ **Not in this clone.** `src/scripts/cfo.zig` was deleted by commit `34a6d6bde` ("cfo: remove from repo", 2026-08-27). Where it lives now is **not verifiable** from this clone. The CHANGELOG still links the old in-repo path (0.16.39 entry). The description below is from the last in-repo version (`git show 34a6d6bde^:src/scripts/cfo.zig`). Treat details as historical.

From that file's header and code:

- Machines loop `git fetch && git reset --hard origin/main && ./zig/zig build scripts -- cfo`. By editing the script in the repo, the team reprograms the whole fleet.
- It keeps `concurrency` fuzzers running at once, with a budget of 60 min per cycle, a seed refresh every 5 min and a timeout of 30 min (a fuzzer that runs longer counts as a **failure**: a hang is a liveness bug).
- A fuzzer fails if it returns a non-zero exit code. This is why the VOPR's exit codes (127, 128, 129) matter, and why `unimplemented` exits 0.
- **Weighted fuzzer choice.** The VOPR variants (`vopr`, `vopr_lite`, `vopr_testing`, `vopr_testing_lite`) have weight 8 each. `lsm_forest` has 4. Most component fuzzers have 1 or 2.
- **Branches.** The budget is split "40:40:20 between main, pull requests, and release". A PR is fuzzed when it has a label like `fuzz lsm_tree`.
- **Results.** Results go to a `devhubdb` repository. Failing seeds are kept in preference to passing ones, and seeds that fail faster are preferred, which acts as a cheap minimization.
- The release process relies on it. From the 0.16.39 CHANGELOG entry: "a TigerBeetle release is tagged on Friday, spends a weekend on the CFO fleet, and is published on Monday."

### 3.8 Fuzzers and assertions are a pair

`TIGER_STYLE.md` ("Safety"): "Assertions are a force multiplier for discovering bugs by fuzzing." A fuzzer with no oracle can only find crashes. Every `assert` in production code becomes an oracle *for free* in every fuzzer that reaches it.

The same section adds a warning: "a fuzzer can prove only the presence of bugs, not their absence". Fuzzing is the last line of defence, after a precise mental model and assertions, not a replacement for them (Concept 30).

## 4. What it means for ashi

### Rust tools, and where each fits

| Tool | What it is | Fit for ashi |
|---|---|---|
| **cargo-fuzz** (libFuzzer) | coverage-guided byte mutation, nightly + sanitizers | Great for **parsers and decoders**: wire header decode, EWAH-like codecs, multi-batch decode, superblock decode. Pair with `arbitrary` to get structure. |
| **arbitrary** | derive `Arbitrary` to turn fuzzer bytes into typed values, e.g. `Vec<Op>` | The bridge from libFuzzer bytes to an operation enum, giving structured, coverage-guided fuzzing. |
| **proptest** | property testing with strategies and **shrinking** | Good for model-based tests run under `cargo test`. `proptest-state-machine` exists for stateful models (verify its current API). Shrinking gives minimal failing sequences, which TigerBeetle lacks beyond "prefer faster failures". |
| **bolero** | one harness API that runs as a proptest-like test, under libFuzzer/AFL/honggfuzz, or with Kani | Attractive: write the harness once, run it in `cargo test` and in a fuzz fleet. |
| **quickcheck** | older property testing | proptest or bolero is usually preferred. |
| **Hand-rolled** (TigerBeetle-style) | `fn main(seed: u64, events_max: Option<usize>)` + own PRNG + model | Fully deterministic, no nightly, no deps (Concept 13), and fits one shared `fuzz` binary and fleet. No shrinking or coverage guidance. |

(These crate descriptions are from general knowledge, not checked against current releases. Confirm features such as `proptest-state-machine` and bolero's engine list before you commit to one.)

**A pragmatic hybrid for ashi:**

1. **Own the harness shape.** Every fuzzer is `pub fn run(seed: u64, events_max: Option<usize>) -> Result<(), Failure>`, registered in one `ashi-fuzz` binary with `smoke` and `canary` entries, exactly like `fuzz_tests.zig`. This is what a fleet scheduler needs.
2. **Generate from a seed by default, using ashi's own PRNG**, so seeds stay stable across crate upgrades.
3. **Optionally** add a second front-end that feeds the same `Vec<Op>` from `arbitrary` under `cargo fuzz`. Then you get coverage guidance and corpus minimization for codecs. Don't make it the only path: libFuzzer corpora aren't "one u64 replays it".
4. **Model-based is the default pattern**, not the exception:

```rust
#[derive(Debug, Clone, Copy)]
enum Op { Reserve { blocks: u32 }, Acquire { r: usize }, Release { a: usize }, Checkpoint }

fn run(seed: u64, events_max: Option<usize>) {
    let mut prng = Prng::from_seed(seed);
    let weights = swarm_weights::<OpKind>(&mut prng);          // some kinds disabled
    let n = events_max.unwrap_or_else(|| prng.exponential(10_000));
    let (mut real, mut model) = (FreeSet::new(blocks), FreeSetModel::new(blocks));
    for _ in 0..n {
        let op = gen_op(&mut prng, &weights, &real);
        let (a, b) = (real.apply(op), model.apply(op));
        assert_eq!(a, b, "op={op:?}");
        assert_eq!(real.count_free(), model.count_free());
    }
}
```

### Rust-specific pitfalls

- **The model must not reuse the implementation's code**, including a shared helper module. In Rust it is tempting to `use crate::free_set::bit_ops`. Don't.
- **`HashMap` iteration in models** is fine for *lookups*. But if the model's output *order* is compared to the real one, use `BTreeMap` or sort. Random iteration order makes seeds non-reproducible (Concept 29, Q4).
- **`debug_assert!`** compiled out in `--release` fuzz builds silently removes your oracles. Fuzz with `debug-assertions = true` plus optimizations (a custom profile, e.g. `[profile.fuzz] inherits = "release"`, `debug-assertions = true`, `overflow-checks = true`). Overflow checks matter especially for a ledger, since u128 arithmetic is the core.
- **Panics vs exit codes.** With `panic = "abort"`, a fleet sees a signal (SIGABRT). Map failures to distinct exit codes (crash, correctness, timeout) so the fleet can triage without parsing logs, and add a canary.
- **Fuzz the real IO adapters**, not only the core. Like `storage_fuzz` and `message_bus_fuzz`, give the io_uring storage and TCP bus a fake syscall layer that injects `EIO`, short reads and writes, and `ECONNRESET`. Otherwise those adapters are tested only by Vortex-style runs.
- **Coverage tests for generators.** Copy the "options swarm coverage" idea: a `#[test]` that runs a few fixed seeds and asserts every error variant of `CreateTransferResult` was produced. In Rust you can enumerate variants with `strum::EnumIter`, or with a hand-written `const ALL: &[Self]`.

**ashi stance:** _to be decided once the drill is done_
Suggest **adapt**: copy the harness contract, the model-based pattern, swarm weights, canary and coverage tests as-is. Add an optional `arbitrary`/`cargo-fuzz` front-end for codecs, because Rust's tools there are good and cheap. Keep seeded, dependency-free generation as the canonical path.

## 5. Drill questions

### Q1. Recall

Name four fuzzers registered in `fuzz_tests.zig` and the oracle each uses. At least one must be a round-trip oracle, one a reference model, and one a fuzzer that targets production code the VOPR mocks out. What are `canary` and `smoke` for?

> _Your answer:_

### Q2. Mechanism

The forest fuzzer's model keeps `transfers_mutable`, `transfers_stashed` and a `log`. Explain what happens to each on (a) a checkpoint and (b) a `crash_after_ticks` reset. What durability bug would a model *without* the stash fail to catch?

> _Your answer:_

### Q3. Back-of-envelope

`free_set_fuzz` draws `events_count = random_int_exponential(prng, blocks_count * 100)`, with `blocks_count = shard_bits × U[1,10]`. `FreeSet.shard_bits` is 4096 (`vsr/free_set.zig:107-109`). What is the mean event count at the smallest and largest `blocks_count`? For an exponential distribution, what fraction of runs exceed 3× the mean? Why is an exponential distribution better than a fixed count for a fleet?

> _Your answer:_

### Q4. What breaks if…

A contributor "simplifies" `tree_fuzz` by using the real `Tree` (with a huge memtable that never flushes) as the model, since "the memtable path is already well tested". What class of bugs can this fuzzer no longer find? Second case: someone makes the state-machine generator skip transfers with `imported = true`. Which existing test fails, and why is it good that a test fails?

> _Your answer:_

### Q5. ashi design (open-ended)

Pick ashi's fuzzing stack:

- hand-rolled seed harness only;
- proptest only;
- bolero;
- a hybrid.

Justify it against these requirements: one-`u64` reproducibility, shrinking, coverage guidance, zero or low dependencies, and a fleet runner. Say which components get which approach, and what ashi's equivalent of `storage_fuzz` and `message_bus_fuzz` looks like.

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** Examples:
- **Round-trip:** `ewah` (`decode(encode(x)) == x`, several word types) and `vsr_multi_batch` (encode/decode of multi-batches).
- **Reference model:**
  - `vsr_free_set` vs `FreeSetModel` (plain bitsets);
  - `lsm_segmented_array` vs `ArrayList`;
  - `lsm_tree` vs a `SortedSegmentedArray`;
  - `lsm_cache_map` vs `AutoHashMap` plus an undo log;
  - `lsm_scan` vs an ordered `ArrayList`;
  - `lsm_forest` vs a mutable/stash/log model;
  - `lsm_manifest_log` vs `ManifestLogModel`.
- **Production code the VOPR mocks:** `storage` (real `StorageType` over fake IO with bad sectors) and `message_bus` (real `MessageBusType` over a fake socket layer with partial, corrupt and failed sends).
- **Invariant-based:** `vsr_superblock` (quorum found, never regresses).
- **Crash-freedom plus coverage:** `state_machine`.
- `canary` fails on purpose when `seed % 100 == 0`, to prove the failure-reporting pipeline works.
- `smoke` runs every fuzzer briefly with seed 123. CI runs it on every change, so fuzzers can't bit-rot.

**Q2.**
- (a) On checkpoint (`forest_fuzz.zig:600-613`), the model applies only the **prefix** of the log up to `checkpointable = op - (op % lsm_compaction_ops) - 1` into `transfers_stashed`. It then drops those entries from the log. Later entries stay in the log, because "each checkpoint persists only a prefix of the log": the in-flight compaction bar is not part of it. `transfers_mutable` is untouched.
- (b) On a crash reset (`storage_reset`, line 615), `mutable` is restored from a clone of `stash` and the log is cleared, because uncheckpointed forest state is not durable at this layer. In the full system, the WAL replay re-applies it.
- Without the stash, the model would expect *all* writes to survive. It would then either report false failures (if the forest correctly loses uncheckpointed writes) or, if the model were changed to "forget everything", it would miss the real bugs. Real bugs it would miss: a checkpoint that persists *less* than it claims (lost acknowledged-checkpointed data), *more* than the checkpoint (state from after the checkpoint leaking into it, which breaks determinism across replicas), or secondary indexes that recover out of sync with the objects.

**Q3.**
- `shard_bits = 4096`, so `blocks_count` ranges from 4,096 to 40,960. So the mean events range from 409,600 to 4,096,000. (The actual code also caps at `events_max orelse 2_000_000`.)
- For an exponential distribution, P(X > 3μ) = e⁻³ ≈ 5%.
- Why exponential suits a fleet:
  - most seeds finish quickly, so you get many distinct configs per CPU-hour, which suits swarm diversity;
  - a fat tail still produces some very long runs that reach deep states (many checkpoints, a nearly full set);
  - a fixed count either wastes time on every run or never goes deep.

**Q4.**
- **Real Tree as the model:** a differential test with the *same* code on both sides cannot see bugs the two share. Lost: bugs in the logic both paths use (key comparison, tombstone handling, scan direction, snapshot visibility), and any bug whose wrong answer is *consistent*. It also stops testing the model's independence, which is the whole point. The forest fuzzer's comment says it outright: "keep the model independent of the LSM / forest implementation." TigerBeetle's tree model is a `SortedSegmentedArray`, a different, simpler structure that has its own `ArrayList`-checked fuzzer. That builds a ladder of trust.
- **Skipping imported transfers:** `test "options swarm coverage"` (`state_machine_fuzz.zig:929`) fails with "uncovered transfer status: imported_event_…" for the import-specific statuses. That is good. Without it, the fuzzer would keep passing while no longer testing a whole feature. A silently weaker fuzzer is worse than a failing one.

**Q5.** A strong answer:
- **Hybrid.**
  - The canonical path is a hand-rolled harness, `run(seed, events_max)`, with ashi's own PRNG, registered in one `ashi-fuzz` binary with `smoke` and `canary`. That gives one-u64 reproducibility, no deps and a trivial fleet runner.
  - Put model-based fuzzers on every stateful component: free set, LSM tree, forest with crash modifier, manifest, cache, ledger state machine with a coverage test over every result code.
  - Codecs and decoders (message header, superblock, multi-batch, any compression) also get a `cargo fuzz` target using `arbitrary`, for coverage guidance and corpus minimization. Optionally use proptest for shrinking on small models in `cargo test`.
- **ashi's equivalent of `storage_fuzz`:** the real `IoUringStorage<S: Syscalls>` over a fake `Syscalls` that returns `EIO` for faulty sectors, short reads and torn writes. Property: reads return written data, or zeros plus a detected checksum failure.
- **ashi's equivalent of `message_bus_fuzz`:** the real bus over fake sockets with partial reads and writes, resets, corrupted bytes and misdirected peers. Property: every message reaches exactly the intended peer, and no frame is accepted with a bad checksum.
- Both require the IO adapters to be generic over a syscall trait. Decide that on day one (Concept 29).

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
