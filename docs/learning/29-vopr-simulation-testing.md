# Concept 29: VOPR, the deterministic simulator

**Module:** M5 Correctness · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/internals/vopr.md` (whole file)
- `../tigerbeetle/docs/internals/testing.md`, section "VOPR Output"
- `../tigerbeetle/docs/internals/HACKING.md`, section "Simulation"
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Determinism" (end) and "Simulation Testing"
- `../tigerbeetle/src/vopr.zig`: `CLIArgs`, `main`, `options_swarm`, `options_lite`, `options_performance`, `Simulator`
- `../tigerbeetle/src/testing/cluster.zig`: `Failure`, `ClusterType`, `tick`, `log_replica`
- `../tigerbeetle/src/testing/cluster/`: `state_checker.zig`, `storage_checker.zig`, `grid_checker.zig`, `manifest_checker.zig`, `journal_checker.zig`, `network.zig`, `message_bus.zig`
- `../tigerbeetle/src/testing/storage.zig` (header comment, `Storage.Options`, `reset`, `ClusterFaultAtlas`)
- `../tigerbeetle/src/testing/packet_simulator.zig` (`PacketSimulatorOptions`, `PartitionMode`)
- `../tigerbeetle/src/testing/reply_sequence.zig`, `../tigerbeetle/src/testing/fixtures.zig`
- `../tigerbeetle/src/state_machine/workload.zig`, `../tigerbeetle/src/state_machine/auditor.zig` (header comments, `Options.generate`)
- `../tigerbeetle/src/stdx/time.zig` (`Time`, `TimeOS`, `TimeSim`), `../tigerbeetle/src/stdx/prng.zig`, `../tigerbeetle/src/stdx/io.zig`, `../tigerbeetle/src/stdx/testing/fuzz.zig` (`parse_seed`)
- `../tigerbeetle/src/tigerbeetle/main.zig` (production wiring), `../tigerbeetle/src/vsr/replica.zig` (`ReplicaType`)
- `../tigerbeetle/src/vortex.zig`, `../tigerbeetle/src/testing/vortex/supervisor.zig`, `../tigerbeetle/src/testing/vortex/faulty_network.zig`
- `../tigerbeetle/build.zig` (`vopr` step, CI invocation)

## 1. The problem

A distributed ledger has two kinds of bugs that matter.

- **Safety bugs:** the cluster does something wrong. Two replicas commit different transfers at the same op (split brain). A committed transfer disappears after a crash. A balance goes negative when the account forbids it.
- **Liveness bugs:** the cluster does nothing. It gets stuck in a view change forever, or a replica can never repair a corrupted block even though a healthy copy exists elsewhere.

Both kinds hide in **rare interleavings**. Think of a message delayed by 2 seconds, arriving just after the sender crashed and restarted, while the receiver's disk returns a corrupt sector for the one header it needs. Unit tests never write that scenario. Real clusters hit it once a year, at 3 a.m., and you cannot reproduce it.

For money, "once a year, unreproducible" is not acceptable. A single lost or doubled transfer is a correctness incident.

## 2. The obvious solution, and why it fails

### Option A: run a real cluster and inject faults (Jepsen-style)

Start 3 processes, kill them, cut network links with `iptables`, and check the history afterwards.

- **Slow.** Real time is real. A 30-second election timeout costs 30 real seconds. To see a rare event that occurs once per 10⁶ seconds of operation, you need ~11.5 days of wall-clock time *per occurrence*.
- **Not reproducible.** Thread scheduling, kernel timers and TCP retransmits differ every run. You see the failure once; you cannot replay it.
- **Shallow storage faults.** It is hard to make a real disk return a bit flip in sector 4,711 exactly when the WAL header is being read.

### Option B: formal model (TLA+)

Model the protocol in TLA+ and model-check it.

- Excellent for finding bugs in the **algorithm**.
- But it checks the model, not your code. The 30,000 lines of Zig can still have an off-by-one in ring-buffer wraparound, a mis-ordered write, or a wrong *assumption* about the disk. TigerBeetle's architecture doc says it directly: "Tools like TLA are invaluable to debug an algorithm. They are of little help if you want to check if your code implements the algorithm correctly or verify underlying _assumptions_." (`docs/ARCHITECTURE.md`, "Simulation Testing").
- Keeping a spec in sync with a fast-moving codebase is a second job.

### What you actually want

Run the **real production code**, with every source of nondeterminism (clock, network, disk, randomness) replaced by a seeded simulation. Then:

- time is a counter, so it runs as fast as the CPU allows;
- one 64-bit seed fully determines the run, so every failure replays exactly;
- faults can be injected at any point, at any rate, including ones real hardware rarely shows.

That is **deterministic simulation testing (DST)**.

## 3. What TigerBeetle does

### 3.1 Lineage

- **FoundationDB** built its database inside a deterministic simulator from day one. The whole cluster ran in one process with simulated network and disk. `docs/internals/vopr.md` names FoundationDB and Antithesis (a company that later sold hypervisor-level DST as a product) as the main inspirations.
- **Dropbox Nucleus** (the sync engine rewrite) used randomized, seeded testing of the sync engine with fault injection. `docs/ARCHITECTURE.md` lists it beside FoundationDB as an inspiration.
- **Swarm testing** (Groce et al., 2012, linked in `docs/ARCHITECTURE.md` "References") is the idea of randomizing the *test configuration*, not only the inputs. See 3.6.

**VOPR** stands for "Viewstamped Operation Replicator", a pun on WOPR from the film *WarGames* (`docs/internals/vopr.md`).

### 3.2 The core trick: one codebase, two worlds

TigerBeetle's replica is generic over everything that touches the outside world. In Zig that means compile-time type parameters (`comptime`), which work like Rust generics with monomorphization.

`src/vsr/replica.zig:144`:

```zig
pub fn ReplicaType(
    comptime StateMachine: type,
    comptime MessageBus: type,
    comptime Storage: type,
    comptime AOF: type,
) type {
```

Production (`src/tigerbeetle/main.zig:21-30`) plugs in real implementations:

```zig
pub const Storage = vsr.storage.StorageType(IO);        // io_uring / kqueue / IOCP
const MessageBus = vsr.message_bus.MessageBusType(IO);  // real TCP
pub const Replica = vsr.ReplicaType(StateMachine, MessageBus, Storage, AOF);
```

The simulator (`src/testing/cluster.zig:63-71`) plugs in fakes:

```zig
pub const Network = @import("cluster/network.zig").Network;
pub const Storage = @import("storage.zig").Storage;          // in-memory, faulty
pub const MessageBus = @import("cluster/message_bus.zig").MessageBus;
pub const Replica = vsr.ReplicaType(StateMachine, MessageBus, Storage, AOF);
```

There is no shared "interface" declaration. The fake must simply have the same method names. Both `Storage` types expose `read_sectors`, `write_sectors`, `flush_sectors`, `on_next_tick`, `reset_next_tick_lsm` and `run` (`src/storage.zig:149-463`, `src/testing/storage.zig:387-682`). Both message buses expose `send_message_to_replica` and `send_message_to_client` (`src/message_bus.zig:909,926`, `src/testing/cluster/message_bus.zig:90,102`).

The four nondeterminism sources and how each is cut:

```
 source        production                              simulation
 ───────────   ─────────────────────────────────────   ─────────────────────────────────────
 disk          StorageType(IO)  (src/storage.zig)      testing.Storage: RAM + fault injection
 network       MessageBusType(IO) (TCP)                testing MessageBus → Network → PacketSimulator
 clock         TimeOS (stdx/time.zig:40)               TimeSim (stdx/time.zig:143), advanced by tick()
 randomness    io.crypto_random_int(u128) → nonce      cluster PRNG → prng.int(u128) → nonce
               (tigerbeetle/main.zig:366)              (testing/cluster.zig:454)
```

**Clock.** `Time` is a small vtable with two methods, `monotonic` and `realtime` (`src/stdx/time.zig:15-37`). It is the one place TigerBeetle uses runtime dispatch for this seam. It costs one indirect call per clock read, which is rare. `TimeSim.tick()` advances monotonic time by exactly one `tick_duration` (`src/stdx/time.zig:260-269`). The cluster sets `tick_duration` to `constants.tick_ms` (`src/testing/fixtures.zig:37`), and `tick_ms` is 10 (`src/config.zig:120`).

**Randomness.** The replica has its own PRNG, but it is seeded from `random_nonce` (`src/vsr/replica.zig:1450`). In production that nonce is the **only** entropy, read once from the OS. In simulation it comes from the cluster's seeded PRNG. So the replica's "random" jitter and scrubber order are replayable too.

**PRNG.** `stdx.PRNG` is xoshiro256++ seeded through SplitMix64 (`src/stdx/prng.zig:110-148`). The header comment explains why TigerBeetle wraps it instead of using Zig's std directly: "remove floating point from the API, to ensure determinism" and "isolate our test suite from stdlib API churn". A PRNG change in the standard library would silently invalidate every saved seed.

**Single thread.** Because TigerBeetle is single-threaded by design (Concept 6), there is no scheduler nondeterminism to tame. The simulator runs **every replica, every client and the network in one OS thread**, one after another, in a fixed order. This is the payoff of Concepts 6, 11 and 12: no threads, no allocation after startup, and explicit callbacks instead of a runtime that picks what runs next.

### 3.3 The tick loop

`Cluster.tick()` (`src/testing/cluster.zig:526-600`) is the heartbeat of the whole simulated world:

```
Cluster.tick():
  loop until nothing advanced:            ← "faster-than-a-tick IO"
      network.step()                      deliver packets whose ready_at has passed
      for each storage: storage.step()    complete reads/writes whose latency elapsed
          + execute pending upgrade       (upgrades happen at checkpoint completion)
  network.tick()
  for each client:  time_sim.tick(); client.tick()
  for each replica:
      time_sim.tick()                     "Time keeps ticking even if a replica is down"
      storage.tick()
      if up and not paused:
          replica.tick()                  timeouts fire here (view change, repair, ...)
          1% of ticks: JournalChecker.check(replica)
          state_checker.check_state(replica)
```

`Simulator.tick()` (`src/vopr.zig:847-862`) wraps that and adds the "chaos monkey" steps: `tick_requests` (maybe send a client request), `tick_upgrade`, `tick_crash` and `tick_pause`.

**Why time runs fast.** A tick is just a loop iteration. If nothing is waiting, a simulated 10 ms takes microseconds of CPU. The VOPR's default patience is `ticks_max_requests: u32 = 40_000_000` (`src/vopr.zig:83`). At 10 ms per tick that is 400,000 simulated seconds, about **4.6 simulated days**, before it declares the cluster stuck. `docs/internals/vopr.md` sums this up: "One minute of VOPR time is equivalent to days of real-world testing." (That ratio depends on the seed and the machine; the code doesn't pin a number.)

### 3.4 Network faults

The simulated network is `PacketSimulator` (`src/testing/packet_simulator.zig:13-50`). Every link is a priority queue ordered by `ready_at`. Faults, all drawn from the seed:

| Fault | Option | Swarm range in `options_swarm` (`src/vopr.zig:450-463`) |
|---|---|---|
| Delay (exponential) | `one_way_delay_min`, `one_way_delay_mean` | min 0–30 ms, mean 30–100 ms |
| Loss | `packet_loss_probability` | 0–30% |
| Duplication (replay) | `packet_replay_probability` | 0–50% |
| Bounded link buffer | `path_maximum_capacity` | 2–20 packets, then random drops |
| Clogging (a link stalls) | `path_clog_probability`, `path_clog_duration_mean` | 0–2% per tick, mean 0–5 s |
| Partitions | `partition_mode`, `partition_symmetry`, `partition_probability` | modes `none`, `uniform_size`, `uniform_partition`, `isolate_single`; symmetric or asymmetric |

Reordering is not a separate knob. It falls out of independent random delays per packet. This is the "weak message-passing contract" from Concept 28 made executable: if the protocol assumed FIFO delivery, the VOPR would break it within seconds.

### 3.5 Storage faults

`src/testing/storage.zig` is an in-memory disk ("In-memory storage, with simulated faults and latency", line 1). Fault types (`Storage.Options`, lines 50-80):

- **Read faults:** a read returns a corrupted sector.
- **Write faults:** a write corrupts a sector.
- **Misdirected writes:** the data lands at the wrong offset (lines 162-186 explain the overlay trick used to model this).
- **Crash faults:** on crash, `reset()` (lines 271-291) cancels pending IO and, with `crash_fault_probability`, corrupts a sector of a write that was in flight. That is a torn write.
- **Latency:** read and write latency distributions, so IO completes out of order.

Swarm ranges (`src/vopr.zig:466-479`): read and write fault probability 0–10%, misdirect 0–10%, crash fault 80–100%.

**The fault atlas: inject only recoverable faults.** Random faults on all replicas would sometimes destroy every copy of a block. Then the cluster *correctly* cannot make progress, and the test reports noise. `ClusterFaultAtlas` (`src/testing/storage.zig:1018`) pre-plans which replica may be faulty in which area, so at least one replica always holds a good copy. From the header comment (lines 4-24):

- superblock: one read/write fault per area, plus one at the target of a pending write during a crash;
- WAL headers and prepares: distributed so "at least one replica will have a valid copy";
- with `replica_count=1`, the WAL can only be corrupted by a crash;
- grid: disabled when `replica_count ≤ 2`.

`options_swarm` enforces the same rule: `.faulty_grid = replica_count > 2` (`src/vopr.zig:487`). The comment explains why R=2 is excluded: a lagging replica may sync from the leader, after which the leader holds the only copy of a block.

This is the storage fault model from Concept 14, made executable. Concept 24 (protocol-aware recovery) and Concept 25 (WAL and grid repair) are exactly what these faults exercise.

### 3.6 Swarm testing: random per-seed configuration

A naive fuzzer picks one config (say, 3 replicas, 1% loss) and varies only the inputs. Bugs that need an unusual config (6 replicas plus 4 standbys, 30% loss, tiny batches) then never appear.

`options_swarm` (`src/vopr.zig:379-527`) derives **the whole configuration from the seed**:

```zig
const replica_count = prng.range_inclusive(u8, 1, constants.replicas_max);   // 1..6
const standby_count = prng.int_inclusive(u8, constants.standbys_max);        // 0..6
const client_count  = prng.range_inclusive(u8, 1, constants.clients_max * 2 - 1);
```

`replicas_max = 6` and `standbys_max = 6` (`src/constants.zig:24,26`). Also randomized per seed:

- batch size limit, and multi-batch count per request;
- LSM compaction block count;
- whether each object cache is enabled at all (`256` or `0` entries);
- every network and storage parameter above;
- crash and restart stability windows;
- request rate and idle periods.

The workload itself is swarm-tested too. `Workload.Options.generate` (`src/state_machine/workload.zig:2152`) picks random weights per operation, for example `create_transfers` 1–100 and `lookup_accounts` 1–20. So some seeds hammer lookups and others are almost all transfers. `stdx/testing/fuzz.zig:34` (`random_enum_weights`) states the principle: "This is swarm testing: some variants are disabled completely, and the rest have wildly different probabilities."

Fixed across seeds (`src/vopr.zig:508-525`):

- crash probability `ratio(2, 10_000_000)` per tick per replica;
- restart probability `ratio(2, 1_000_000)`;
- reformat on restart 30%;
- `requests_max = constants.journal_slot_count * 3`.

The VOPR is compiled with the `test_min` config (`src/vopr.zig:74`). That config shrinks everything so that edge cases come quickly:

- `journal_slot_count = journal_slot_count_min` (`src/config.zig:258`), which is `2 * 4096/256 = 32` slots (`src/config.zig:179`);
- `block_size = sector_size` (4 KiB);
- `pipeline_prepare_queue_max = 4`;
- `lsm_compaction_ops = 4`.

So by default a run sends **96 requests**, and the 32-slot WAL wraps three times. Wraparound, checkpointing and WAL repair all happen in every run.

Three modes exist (`src/vopr.zig:157-162`):

- default **swarm**;
- **`--lite`** (`options_lite`, line 529): 3 replicas, 0 standbys, and it skips the liveness check;
- **`--performance`** (`options_performance`, line 537): 6 replicas, no faults, and a fixed workload seed of 92, used to compare message counts between commits.

### 3.7 Safety mode, then liveness mode

A run has two phases (`src/vopr.zig:268-370`).

```
                 ┌─────────────── SAFETY phase ───────────────┐   ┌──── LIVENESS phase ────┐
 seed ─► config ─►  faults ON: crash, restart, pause, reformat, ├──►  pick a "core" set     ├─► PASSED
                 │  partitions, loss, disk corruption, upgrades │   │  heal core: restart,   │   or exit code
                 │  checkers run every tick                     │   │  unpartition, disable  │
                 │  until all requests replied (or 40M ticks)   │   │  storage faults        │
                 └──────────────────────────────────────────────┘   │  must converge within  │
                                                                    │  10M ticks             │
                                                                    └────────────────────────┘
```

**Safety.** The comment at `src/vopr.zig:268`: "replicas crash and restart; at any given point in time arbitrarily many replicas may be crashed, but each replica restarts eventually. The cluster must process all requests without split-brain." During this phase you cannot demand progress, because the faults might legitimately prevent it. You can only demand that nothing *wrong* happens. The checkers enforce that on every tick.

**Liveness.** After the requests are done, or after 40M ticks without a reply, `transition_to_liveness_mode` (`src/vopr.zig:893-924`) does the following:

- picks a **core**, a set of replicas of at least view-change-quorum size (`random_core`, line 1726);
- restarts any core replica that is down and unpauses paused ones;
- calls `storage.transition_to_liveness_mode()` to stop storage faults on the core;
- heals the network between core members, with 1 ms delay and no loss (`src/testing/cluster/network.zig:132-138`);
- sets crash, restart, reformat, pause and upgrade probabilities to zero;
- leaves failures of non-core replicas **permanent**.

Then the core must converge: `Simulator.pending()` (`src/vopr.zig:783-845`) must return `null` within `ticks_max_convergence` (10M ticks). That means no pending requests, all core replicas on the same release, the same commits, all prepares repaired, sync done, and an identical `checkpoint_id`.

If the safety phase timed out, the code uses a *full* core instead: "Run a fully-connected core of replicas to repair all faulty grid blocks, headers, and prepares that can be repaired."

**Not every stall is a bug.** Before reporting a liveness failure, `cluster_recoverable` (`src/vopr.zig:865-883`) asks whether convergence was even possible. Examples: is a prepare or grid block missing from *every* core replica, or did a reformat lose data? If so, it logs "no liveness, …" and the run is not counted as a failure. Only a recoverable-but-stuck cluster is a liveness bug. TigerBeetle's blog post "Simulation Testing for Liveness" (linked from `vopr.md`) describes this design.

### 3.8 The checkers

Assertions inside production code are the first line of defence (Concept 30). They stay on in production, and the VOPR refuses to build without them: `comptime assert(builtin.mode == .Debug or builtin.mode == .ReleaseSafe)` (`src/vopr.zig:139`). It also requires `constants.verify`, which enables extra expensive checks. On top of that, the simulator has **checkers** that see across replicas. No single replica could check these.

| Checker | File | What it verifies |
|---|---|---|
| **StateChecker** | `testing/cluster/state_checker.zig` | Builds the canonical commit history: a list of prepare headers, one per op. Every time any replica's `commit_min` advances, it checks that the new header's `parent` is the previous canonical checksum (a hash chain, see Concept 15). It checks that the op is either already in history with the same checksum, or is the next op and matches an in-flight client request. It also checks the release matches. Two replicas committing different ops at the same number means split brain, and an assert fires. Also `assert_cluster_convergence` (line 312). |
| **StorageChecker** | `testing/cluster/storage_checker.zig` | "Verify deterministic storage" (line 1). At each compaction bar and checkpoint, acquired grid blocks, the superblock `vsr_state.checkpoint` and client replies must be **byte-for-byte identical across replicas** (physical determinism, Concept 20). It explicitly does not compare WAL headers or prepares, superblock headers, or free grid blocks, and lines 14-20 say why. |
| **GridChecker** | `testing/cluster/grid_checker.zig` | `assert_coherent` (line 22): for a given `(checkpoint_id, block_address)`, every replica reads the same block checksum. A block from the durable previous checkpoint must not change while the current checkpoint is not yet durable. This is the copy-on-write guarantee from Concept 17. |
| **ManifestChecker** | `testing/cluster/manifest_checker.zig` | "Verify that the ManifestLevels tables are constructed consistently across replicas and after recovering from a restart" (line 1). Compares cumulative checksums of the LSM manifest per checkpoint (Concept 18). |
| **JournalChecker** | `testing/cluster/journal_checker.zig` | WAL invariants on a clean journal: redundant headers match prepares, the WAL matches `journal.headers`, and sector padding is zeroed. Run on 1% of ticks "for performance" (`cluster.zig:589-592`). |
| **Workload + Auditor** | `state_machine/workload.zig`, `state_machine/auditor.zig` | Semantic correctness of the ledger, below. |
| **AOF validate** | `src/aof.zig:273`, called at `vopr.zig:362` | After liveness, each core replica's append-only file must contain the full hash chain up to the last committed checksum. |
| **Leak check** | `vopr.zig:93-101` | `DebugAllocator` panics with "memory leaked" on exit. |

### 3.9 The workload generator

`src/state_machine/workload.zig` header: "The Workload drives an end-to-end test: from client requests, through consensus and the state machine, down to the storage engine, and back."

Clever bits:

- **Transfer IDs encode their own fate.** "`Transfer.id` is a deterministic, reversible permutation of an ascending index. With the transfer's index as a seed, the Workload knows the eventual outcome of the transfer." So the workload can check a transfer created thousands of ops ago **without storing it**. That keeps memory fixed for long runs (goal: "Run in a fixed amount of memory").
- **`user_data` is a checksum** of the rest of the transfer, so a lookup can check every field it gets back.
- Each planned transfer has an expected outcome of `success`, `failure` or `unknown` (lines 38-46). Invalid transfers are sent on purpose and **must** be rejected. `unknown` covers real races, such as a post-pending racing its timeout.
- **The Auditor** (`auditor.zig`) is a **reference model**: "constructs the expected state of its corresponding StateMachine from requests and replies. It validates replies against its local state." It tracks balances and pending transfers and checks every reply.

The Auditor "expects replies in ascending commit order". But replies arrive out of order over the faulty network. `ReplySequence` (`src/testing/reply_sequence.zig`) reorders them by op. `on_cluster_reply` (`src/vopr.zig:1340`) feeds them to the workload one op at a time, checking each against the StateChecker's canonical header for that op.

Build option `-Dvopr-state-machine=testing` swaps in a tiny `echo` state machine (`src/testing/state_machine.zig:18`). That makes consensus testing cheaper. CI runs both (`build.zig:644-652`).

### 3.10 Seeds and reproducibility

- `zig build vopr` picks a random seed from the OS and requires `-OReleaseSafe` for speed (`src/vopr.zig:127-149`).
- `zig build vopr -- 123` replays seed 123 exactly (`docs/internals/HACKING.md`).
- The seed alone is not enough. You need **seed + git commit**, because any code change shifts the PRNG call sequence (`vopr.md`: "the seed and Git commit hash can be used to replay").
- CI passes the **commit hash itself** as the seed: `"vopr", "-Dvopr-state-machine=...", "-Drelease", "--", options.git_commit` (`build.zig:646-651`). `parse_seed` (`src/stdx/testing/fuzz.zig:90-108`) accepts a 40-character hex hash and truncates it to `u64`. Comment: "we run it with different, 'random' seeds, but the failures remain reproducible just from the commit hash!"
- The derivation is a tree. The main seed seeds `prng`. `prng` draws separate sub-seeds for cluster, network and storage (`.seed = prng.int(u64)`). A failing seed can then be replayed with `-Dvopr-log=full` for full debug logs.

**Exit codes** (`src/testing/cluster.zig:48-53`):

| Code | Meaning |
|---|---|
| 127 | crash: any assertion failure |
| 128 | liveness: no convergence |
| 129 | correctness: a checker failed |
| 0 | success, *or* `unimplemented(...)` (`vopr.zig:1717`): a known-unsupported scenario, so it "exits with status 0, to make it easy to find 'real' failures" |

A fuzzing fleet can sort failures by exit code without parsing logs.

### 3.11 Reading VOPR output

With `-Dvopr-log=short` (the default, `build.zig:223-227`), the VOPR prints one line per replica state transition (`log_replica`, `src/testing/cluster.zig:1054-1170`):

```
 3 [ /    .        3V  71/_99/_99C  68:_99Jo  0/_0J!  68:_99Wo <__0:__0> v1:2   183Ga  0G!   0G?   0/4Pp  0/3Rq
 │ │ │    │        │   │  │   │    │   │     │  │    │   │     │   │     │ │    │      │     │     │     └ request queue / max
 │ │ │    │        │   │  │   │    │   │     │  │    │   │     │   │     │ │    │      │     │     └ prepare queue / max (primary only)
 │ │ │    │        │   │  │   │    │   │     │  │    │   │     │   │     │ │    │      │     └ grid blocks being repaired
 │ │ │    │        │   │  │   │    │   │     │  │    │   │     │   │     │ │    │      └ grid reads waiting on other replicas
 │ │ │    │        │   │  │   │    │   │     │  │    │   │     │   │     │ │    └ grid blocks acquired
 │ │ │    │        │   │  │   │    │   │     │  │    │   │     │   │     │ └ max release bundled
 │ │ │    │        │   │  │   │    │   │     │  │    │   │     │   │     └ running release
 │ │ │    │        │   │  │   │    │   │     │  │    │   │     └───┴ sync_op_min:sync_op_max
 │ │ │    │        │   │  │   │    │   │     │  │    └───┴ oldest:newest prepare actually in the WAL
 │ │ │    │        │   │  │   │    │   │     └──┴ faulty / dirty journal headers
 │ │ │    │        │   │  │   │    └───┴ journal op min:max (in-memory headers)
 │ │ │    │        │   │  │   └ commit_max (known committed in cluster)
 │ │ │    │        │   │  └ commit_min (applied locally)
 │ │ │    │        │   └ op_checkpoint
 │ │ │    │        └ view
 │ │ │    └ status, in replica's own column: . normal  v view_change  r recovering  h recovering_head  # down
 │ │ └ role: / primary  \ backup  | standby  ~ syncing  # crashed  F reformatting
 │ └ event: ! crash  ^ recover  ' ' commit  $ sync  X reformat  [ checkpoint start  ] checkpoint done
 └ replica index
```

How to read a failing run:

1. Find the last lines before the panic.
2. Look for a replica whose `commit_min` trails `commit_max` for a long time. It is stuck repairing.
3. Look for a large `J!` faulty count, or a non-zero `G?`. Those mean corruption it cannot repair.
4. Use the status column to spot view-change storms: many `v` with an increasing `V`.

> ⚠️ **Docs vs code** (`docs/internals/testing.md` vs `src/testing/cluster.zig:1119-1163`). The code wins:
> - Column 8: the docs text says `0/1Jd`. The code prints `J!` (`{faulty}/{dirty}J!`), as the docs' own example shows.
> - Column 13: the docs say it counts `grid.read_remote_queue`. The code prints `replica.grid.read_global_queue.count()`.
> - Column 16: the docs say `0/3Pq`. The code prints `Rq`, again matching the docs' example.
> - Column 15: the docs say "`1/4Pp` indicates that the primary's pipeline has 2 prepares queued". It means 1 prepare queued out of 4.
> - Column 4: the docs list an `s` (sync) status. `log_replica` has no `s`. Syncing shows as role `~`, and a down replica shows `#` in the status column (not listed in the docs).
> - Column 11: the code prints only the **patch** component of the release (`replica.release.triple().patch`). That works only because the VOPR's fake releases are `0.0.1`…`0.0.3` (`src/vopr.zig:43-56`).
>
> Also, `docs/internals/vopr.md` says the simulator "commits several hundred batches". The swarm default is `requests_max = journal_slot_count * 3` = **96 requests** under `test_min` (the `--performance` mode uses ×8 = 256). The figure can be raised with `--requests-max`.

### 3.12 What the VOPR does *not* cover

- **Clock skew.** `TimeSim` supports linear, periodic, step and non-ideal clock offsets (`src/stdx/time.zig:180-185`). But the VOPR cluster creates every replica clock with `fixtures.init_time(.{})`, which means linear with zero drift (`src/testing/cluster.zig:262`, `src/testing/fixtures.zig:36-42`). Clock offsets are only exercised by the clock's own tests (`src/vsr/clock.zig:621-838`, for example `.offset_type = .non_ideal`). I could not find clock faults anywhere in the VOPR swarm.
- **The real IO layer, real TCP and real process boundaries.** The simulator swaps these out, so bugs *in* `StorageType(IO)`, `MessageBusType(IO)` or io_uring handling are not covered here. That is Vortex's job.
- **Client libraries** in other languages. Also Vortex.

### 3.13 Vortex: the non-deterministic big brother

`src/vortex.zig:1-4`: "Run a cluster of TigerBeetle replicas, a client driver, a workload, all with fault injection, to test the whole system. On Linux, Vortex runs in a Linux namespace where it can control the network."

| | VOPR | Vortex |
|---|---|---|
| Process model | whole cluster in **one thread** | each replica and the client driver are **real child processes** (`supervisor.zig:7-9`) |
| Code under test | replica logic with fake IO | the real `tigerbeetle` binary: real io_uring, real TCP, real files |
| Determinism | fully deterministic from seed + commit | "Vortex is non-deterministic, but providing a seed can still help constrain the scenario" (`vortex.zig:35`) |
| Time | simulated; days per minute | real (`--test-duration`, default 1 minute, `vortex.zig:25`) |
| Network faults | loss, replay, delay, partitions, clogging | a proxy per replica that adds delay and corruption; "Others not implemented: duplication, reordering, rate" (`faulty_network.zig:39`) |
| Process faults | crash/restart, pause, reformat, upgrade | terminate, restart, pause (SIGSTOP), resume, replica and cluster upgrades (`supervisor.zig` `tick_faults`) |
| Disk faults | rich (corrupt, misdirect, torn) | none yet: "filesystem faults" and "clock faults" are listed as future work (`supervisor.zig:33-34`) |
| Clients | Zig client in-process | pluggable drivers: Zig, Java, Rust (`src/testing/vortex/*_driver`) speaking a binary stdio protocol |
| Platforms | any | Linux only (`vortex.zig:48-61`) |
| Scenarios | swarm / lite / performance | `default`, `upgrade`, `recover` (`vortex.zig:39-43`) |

The two tools split the work. The VOPR tests **logic** exhaustively and reproducibly. Vortex tests the **glue** the VOPR mocks out: syscalls, sockets, multiversion binary exec (Concept 32) and the client libraries.

### 3.14 Why simulation beats TLA+ for *this* job

- **It tests the code that ships,** not a model of it. The VOPR links `vsr.ReplicaType`, the same function production uses.
- **It tests assumptions.** "Disk writes may be misdirected" is a fault-model *assumption*. TLA+ would accept whatever you assume. The VOPR forces the implementation to cope.
- **It scales with hardware,** not with proof effort. More cores means more seeds per hour. `ARCHITECTURE.md` mentions "a thousand CPU cores" (I could not verify the current fleet size from the repo).
- **Failures are concrete,** a seed you can step through in a debugger, not a counterexample trace in a different language.
- **The trade-off:** DST is sampling, not exhaustive search. A bug that needs a 1-in-10¹² interleaving may never be hit. Model checking a small spec *does* cover all states up to its bounds. Many teams use both. TigerBeetle's docs rank simulation as "the most important technique deployed".

## 4. What it means for ashi

DST is an **architecture decision**, not a test library. You cannot bolt it onto a codebase that calls `Instant::now()`, spawns threads or uses a global async runtime. It has to be designed in from the first commit.

### Rust features that help

- **Generics with monomorphization** match Zig's `comptime` type parameters. `Replica<S: Storage, N: Network, C: Clock>` compiles to two specialized copies, with zero runtime cost.
- **Traits as the seam.** Define narrow traits:
  - `Storage`: `read_sectors`, `write_sectors`, `flush`, `on_next_tick`;
  - `MessageBus`: `send_to_replica`, `send_to_client`;
  - `Clock`: `monotonic`, `realtime`.
  A `&mut dyn Clock` is fine for the clock, as TigerBeetle shows, because clock reads are rare.
- **Explicit PRNG passing.** One `struct Prng([u64; 4])` (xoshiro256++), with `from_seed(u64)` via SplitMix64, owned by the simulator and passed down. Implement it yourself (about 50 lines) so crate upgrades can't change seed meaning. That is the same reason `stdx/prng.zig` exists.
- **Completion-based IO with explicit state machines** (Concept 12) instead of `async fn`. A hand-written callback/state-machine replica is trivially steppable. An `async` replica needs an executor, and the executor's scheduling order becomes another nondeterminism source.

### Rust features that hurt, and the fix for each

| Hazard | Why it breaks determinism | Fix |
|---|---|---|
| `std::collections::HashMap` with the default `RandomState` | iteration order differs per process | use a fixed-seed hasher (e.g. `BuildHasherDefault<FxHasher>`), or ban iteration over hash maps; enforce with `clippy::disallowed_types` |
| `Instant::now()`, `SystemTime::now()` | real time leaks in | allow these only inside the production `Clock` impl; `clippy::disallowed_methods` in `clippy.toml` everywhere else |
| `rand::thread_rng()`, `getrandom` | OS entropy | ban; one nonce read at startup, as in `tigerbeetle/main.zig:366` |
| `std::thread::spawn`, rayon, `tokio::spawn` | OS scheduler order | single-threaded core; ban in the replica crate |
| `#[tokio::main]` / global runtime | timers and IO bound to real OS | no global runtime in core crates; at most an adapter crate implementing the traits |
| Dependencies with hidden threads or randomness | a crate four levels deep calls `Instant::now()` | few dependencies (Concept 13); audit with `cargo tree` |
| `f64` in logic | usually deterministic on one platform, but fragile across targets and compiler flags | integers only in replica and PRNG APIs, like TigerBeetle's `Ratio` |
| Panic = unwind, `catch_unwind` | a half-mutated state is reused after a panic | one seed per **process**; `panic = "abort"`; map assert / liveness / correctness to exit codes as the VOPR does |
| `debug_assert!` | vanishes in release, but you want the simulator fast *and* checked | build the simulator with `debug-assertions = true` plus `opt-level = 3` (a custom Cargo profile), the analogue of ReleaseSafe |

### Prior art in Rust, and why it is not enough on its own

These are from general knowledge. Their current feature sets were not verified against source here, so check each crate's current docs before deciding.

- **madsim:** a drop-in deterministic replacement for the tokio API (and some ecosystem crates) via `cfg(madsim)`. It is the best fit if you are already on tokio. It intercepts time, randomness and the network. But the storage fault model is thin compared with TigerBeetle's (no misdirected writes, torn writes or fault atlas), and correctness still depends on every dependency being covered by its shims.
- **turmoil** (tokio-rs): simulates hosts and a network in one thread, with partitions, delay and loss. It is good for network faults. Disk faults are limited or experimental. It is also a test harness, not a whole-system design.
- **shuttle** (AWS): randomized and PCT scheduling for **concurrency** bugs between threads. It is useful only if you have threads, which a TigerBeetle-style core doesn't.
- **loom:** exhaustive exploration of thread interleavings and memory orderings for small lock-free kernels. It fits a lock-free ring buffer between the IO thread and the core. It cannot handle whole-system tests.
- **Antithesis:** a language-agnostic deterministic hypervisor (a commercial service). It works on any binary, but it is a service, not part of your codebase or your developer loop.

**Why they fall short of TigerBeetle-grade DST:**

1. They make an *existing async program* deterministic. TigerBeetle made the *program itself* deterministic by construction: single thread, static allocation, explicit IO interfaces.
2. None ships a storage fault model that is aware of your on-disk layout (WAL zones, superblock copies, grid). That model has to be yours.
3. None gives you the cross-replica checkers (state, storage byte-identity, grid, manifest) or the safety-then-liveness protocol. Those are domain logic.
4. Seed stability across crate upgrades isn't guaranteed.

**Suggested ashi shape:**

```
crates/
  ashi-core      #![no_std]-ish: replica, state machine, LSM; generic over traits; no tokio, no threads
  ashi-io        production impls: io_uring Storage + MessageBus, OS Clock
  ashi-sim       SimStorage (RAM + faults + fault atlas), SimNetwork (packet simulator), SimClock,
                 Prng, Cluster::tick(), checkers, workload + reference model, `ashi-vopr` binary
```

Make `ashi-sim` the **first** consumer of `ashi-core`, before `ashi-io` exists. If the first replica test is a simulation, determinism leaks show up on day one.

**ashi stance:** _to be decided once the drill is done_
Suggest **adopt**: copy the architecture wholesale (generic seams, single-threaded tick loop, swarm config, safety→liveness, cross-replica checkers, exit codes). The Rust DST crates solve a different problem (taming tokio), and ashi shouldn't have tokio in its core.

## 5. Drill questions

### Q1. Recall

Name the four sources of nondeterminism the VOPR replaces. For each, give the production type and the simulation type, or the production call and the simulation call.

> _Your answer:_

### Q2. Mechanism

Why does the VOPR run a safety phase *then* a liveness phase, instead of checking for progress all the time? And why does it call `cluster_recoverable` before reporting a liveness failure? What would happen to the signal-to-noise ratio of the fuzzing fleet without it?

> _Your answer:_

### Q3. Back-of-envelope

The VOPR uses `test_min`: `sector_size = 4096`, `@sizeOf(Header) = 256`, `journal_slot_count = journal_slot_count_min = 2 * sector_size / @sizeOf(Header)`, `requests_max = journal_slot_count * 3`, `tick_ms = 10`, `ticks_max_requests = 40_000_000`.

- (a) How many requests does a default swarm run send? How many times does the WAL wrap?
- (b) How much simulated time can pass with no reply before the VOPR gives up on the safety phase?
- (c) Why is a tiny WAL a *feature* for testing, not a compromise?

> _Your answer:_

### Q4. What breaks if…

An ashi contributor stores open client sessions in a `std::collections::HashMap<u128, Session>` with the default hasher. On view change, the replica iterates the map to rebuild a reply table. All unit tests pass. What breaks in the simulator, and what breaks in production? Then a second case: `ClusterFaultAtlas` is removed and grid faults are injected independently per replica. What does the fleet start reporting?

> _Your answer:_

### Q5. ashi design (open-ended)

Sketch ashi's seams: the trait signatures for storage, network and clock, and how the PRNG flows. Then decide:

- Should the production network layer be tokio, a hand-written io_uring loop, or tokio *only* in an adapter crate?
- What concrete lint or CI rule would stop determinism leaks from being merged?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- **Disk:** `StorageType(IO)` (`src/storage.zig`) becomes `testing/storage.zig` `Storage`, which is in-memory with fault injection.
- **Network:** `MessageBusType(IO)` (real TCP) becomes `testing/cluster/message_bus.zig` `MessageBus`, which feeds `Network` and `PacketSimulator`.
- **Clock:** `TimeOS` becomes `TimeSim`, both behind the `Time` vtable (`src/stdx/time.zig`).
- **Randomness:** `io.crypto_random_int(u128)` for the replica's `random_nonce` (`tigerbeetle/main.zig:366`) becomes `prng.int(u128)` from the seeded cluster PRNG. The replica's own PRNG is seeded from that nonce.
- (Thread scheduling is not on the list because there is only one thread. Single-threadedness is what removes it.)

**Q2.** While faults are active, lack of progress can be *correct*: a partition can leave no quorum, or crashed replicas can hold the only good copies. So during faults you can only check safety (no divergence, no lost commits). Once faults stop for a core of at least a view-change quorum, the protocol *promises* progress, and failing to converge is then a bug.

`cluster_recoverable` filters out stalls that were physically unavoidable. Examples: a prepare or block missing on every core replica, a core lacking a quorum because of `recovering_head`, or data lost to a reformat. Without it, the fleet would flood with "liveness failures" that are really "the test destroyed all copies". Real bugs would drown in noise, and developers would learn to ignore red runs. The fault atlas (Q4) attacks the same problem from the other side, by not creating unrecoverable faults in the first place.

**Q3.**
- (a) `journal_slot_count = 2 × 4096/256 = 32`, so `requests_max = 96`. The WAL wraps about 3 times. Each request can carry several batches (multi-batch), and the cluster also inserts its own ops (register, pulse, upgrade), so op count ≥ 96.
- (b) 40,000,000 ticks × 10 ms = 400,000 s ≈ 111 hours ≈ **4.6 days** of simulated time. Note that the tick counter resets to 0 whenever a reply arrives (`vopr.zig:276-282`), so this is the patience *per stall*, not total run length.
- (c) Rare production events become routine every run: WAL wraparound, slot reuse, checkpoint triggers (every few ops with `lsm_compaction_ops = 4`), WAL repair across a wrap, and state sync when a replica falls more than a WAL behind. With the production WAL size, a run would need many thousands of requests to reach a single wrap.

**Q4.**
- **HashMap in the simulator:** `RandomState` gets fresh OS entropy per process. Iteration order differs between two runs of the *same seed*, so a failing seed may not reproduce. That destroys the core property of DST. It can also hide bugs that depend on order.
- **HashMap in production:** each replica iterates in a different order. If that order influences anything that is persisted or replicated (reply table layout, the order of writes into the grid), replicas lose **physical determinism** (Concept 20). The StorageChecker would flag differing bytes, but only if the simulator itself is deterministic enough to be trusted, which it now isn't. Fix: fixed hasher, or sort before iterating, or a structure with defined order. Ban it with `clippy::disallowed_types`.
- **No fault atlas:** with independent faults, some seeds corrupt the same grid block (or the same WAL slot) on every replica. The data is truly gone, the cluster correctly halts, and the fleet reports liveness failures, or `cluster_recoverable` reports "no liveness, N blocks are not available in core" on a large share of seeds. That wastes CPU and buries real bugs. The atlas guarantees at least one good copy, so a stall always means a bug.

**Q5.** A good answer includes:
- `trait Storage { fn read(&mut self, zone, offset, buf, cb); fn write(...); fn tick(&mut self); }`, with completions delivered through the tick loop, not futures.
- `trait Bus { fn send_to_replica(&mut self, r: u8, m: MessageRef); fn send_to_client(&mut self, id: u128, m: MessageRef); }`
- `trait Clock { fn monotonic(&self) -> Instant; fn realtime(&self) -> u64; }`
- `Replica<S: Storage, B: Bus>` generic, with `&dyn Clock` acceptable.
- A single PRNG seeded once. The replica gets a nonce, not an `Rng` handle to the OS.
- Network choice: keep tokio, if used at all, only in an adapter crate outside `ashi-core`. The core must not know about it. Ideally a hand-written io_uring loop (Concept 12), but the critical point is that the core never depends on the runtime.
- Enforcement ideas:
  - `clippy.toml` `disallowed-methods = ["std::time::Instant::now", "std::time::SystemTime::now", "std::thread::spawn", "rand::thread_rng"]` and `disallowed-types = ["std::collections::HashMap", "std::collections::HashSet"]` for the core crate;
  - a CI job that runs N seeds and also **runs one seed twice and diffs the logs** (a determinism self-test);
  - `cargo deny` or a manual allowlist of dependencies.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
