# Concept 6: Why a single thread wins (COST)

**Module:** M1 Single-node speed · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Single Thread" (lines 168–187), "Embracing Concurrency" (436–465), "io_uring" (467–487), "Zig" (251–279) and "Simulation Testing" (309–318)
- `../tigerbeetle/docs/concepts/performance.md`, section "Single Threaded By Design"
- `../tigerbeetle/src/tigerbeetle/main.zig` (the replica's main loop)
- `../tigerbeetle/src/stdx/io/linux.zig` (`IO.run_for_ns`)
- `../tigerbeetle/src/vsr/replica.zig` (`tick`, `commit_dispatch`, `on_prepare`)
- `../tigerbeetle/src/lsm/groove.zig` (`PrefetchContext`)
- `../tigerbeetle/src/testing/cluster.zig` (`Cluster.tick`)
- `../tigerbeetle/src/config.zig`, `../tigerbeetle/src/constants.zig`
- Frank McSherry, Michael Isard, Derek Murray, "Scalability! But at what COST?", HotOS 2015 (linked from the docs; the paper itself is not in the clone)
- Line numbers are from the clone at commit `a2a3c70`.

## 1. The problem

A ledger has to apply transfers in **one agreed order**. Each transfer reads two balances, checks rules such as "debits must not exceed credits", and writes two balances.

The order matters for correctness. If transfer A and transfer B both debit the same account, the result of B depends on whether A ran first. A replicated ledger must also apply the same order on every replica (Concept 20).

The reflexive way to go fast is to use every core. A modern server has 32–128 of them. Leaving 127 idle feels wasteful.

For a financial ledger the question is: does adding threads buy throughput, or does it only buy coordination overhead?

## 2. The obvious solution, and why it fails

The obvious design is a thread pool with one lock per account. A worker thread takes a transfer, locks the debit and credit accounts, updates them, and unlocks.

### Hot accounts serialize everything anyway

Concept 1 showed that account usage is Pareto-distributed. Suppose every transfer also credits one `fees` account.

- Every worker must take the `fees` lock.
- The workers queue on that one lock. The workload is serial again, but now it pays for locking.
- The lock's cache line moves between cores on every handoff. This "cache-line ping-pong" typically costs on the order of 100 ns per handoff on current server CPUs. That figure is a general hardware rule of thumb, not a TigerBeetle measurement.

Back-of-envelope:

```
target:           1,000,000 transfers/s on the hot account
time budget:      1 µs per transfer
lock handoff:     ~0.1 µs (cross-core cache-line transfer, rough)
useful work:      a few hash lookups + compares + adds, well under 1 µs if data is in cache
```

The handoff alone eats a large fraction of the budget, and it buys nothing: the hot account still processes one transfer at a time.

### More problems pile up

- **Deadlocks.** Two transfers lock accounts in opposite orders. You need a global lock order. Linked events (a chain of transfers that succeed or fail together) need many locks at once.
- **Determinism is lost.** Threads interleave differently on each run and on each replica. A replicated state machine needs every replica to reach byte-identical state (Concept 20). A parallel executor would have to prove its result equals the log's serial order. That means dependency tracking per transfer, which is more synchronization.
- **Testing gets hard.** A thread interleaving bug may appear once in a billion runs and never reproduce.

### The COST lens

McSherry, Isard and Murray's HotOS 2015 paper "Scalability! But at what COST?" defines **COST**: the *Configuration that Outperforms a Single Thread*. It is the number of cores a parallel system needs before it beats a competent single-threaded implementation of the same task.

- A system can scale "linearly" from 1 to 100 cores and still be slower than one well-written thread. Scaling curves hide that.
- The paper reports parallel graph-processing systems that needed many cores to match a single laptop thread, and some that never matched it. A system that never beats one thread has **unbounded COST**.
- (These paper details are from the paper, not from the TigerBeetle clone, which only links to it.)

TigerBeetle applies this lens to its own workload:

> "Channeling Frank McSherry's paper, our claim is that financial transaction processing is an infinite-COST problem." (`docs/ARCHITECTURE.md:175–177`)

**Infinite COST** means: because hot accounts sequentialize the work, no number of cores lets a parallel ledger beat a good single-threaded one. The overhead of synchronization "tends to dominate useful work" (`ARCHITECTURE.md:175`).

## 3. What TigerBeetle does

### 3.1 One thread owns all replica state

TigerBeetle runs each replica as one event loop on one thread. The replica has no locks and no shared mutable state. The few atomics in shared code sit outside the replica's data path, for example a one-time checksum seed initialization (`src/vsr/checksum.zig:43–44`).

The production main loop is three lines (`src/tigerbeetle/main.zig:591–594`):

```zig
while (true) {
    replica.tick();
    try io.run_for_ns(constants.tick_ms * std.time.ns_per_ms);
    emit_stats(tracer, &io.stats);
}
```

- `replica.tick()` (`src/vsr/replica.zig:1525`) advances logical time: timeouts, heartbeats, retries. Time is counted in ticks, not read from the OS.
- `io.run_for_ns(...)` (`src/stdx/io/linux.zig:100`) submits queued IO to io_uring, waits up to one tick for completions, and runs their callbacks. All callbacks run on this same thread.
- `tick_ms` is 10 (`src/config.zig:120`). So the loop wakes at least every 10 ms. It runs callbacks as soon as IO completes.

Only **one replica**, the primary, decides order. Every replica then executes the same log on its own single thread. The docs are explicit: "Adding more nodes can therefore increase reliability, but not throughput." (`docs/concepts/performance.md`)

⚠️ **Docs vs code (nuance).** "Single-threaded" means the replica's state and event loop. The process is not strictly one OS thread in every mode:

- `main.zig:78` creates a `std.Io.Threaded` instance. `src/stdx/io/linux.zig:32` documents it as "used for blocking file system operations (e.g. `aof_blocking_*`)".
- `main.zig:540–559` spawns a stdin watchdog thread, but only when started with `--addresses=0` (let the OS pick a port), which test harnesses such as `src/testing/tmp_tigerbeetle.zig:87–94` use.
- The client library (`src/clients/c/tb_client/context.zig`) runs its own IO thread. That is the client, not the server.

None of these threads touch ledger state.

### 3.2 Concurrency without parallelism

Single-threaded does not mean "one thing at a time". The docs: "TigerBeetle embraces concurrency. Sequential execution is the exception." (`ARCHITECTURE.md:445`)

Terms:
- **Parallelism:** several CPU cores executing instructions at the same moment.
- **Concurrency:** several operations *in progress* at the same moment, even if one core drives them all.

TigerBeetle keeps many IOs in flight while the CPU does one thing at a time:

```
one core ───────────────────────────────────────────────────────────────▶ time
  CPU:   [submit 32 reads][ … other work … ][run callbacks][commit batch]
  disk:        ├─read─┤ ├─read─┤ … up to 32 in flight …
  NIC:   ├─prepare to replica 1─┤ ├─prepare to replica 2─┤ …
```

Concrete places (Concept 12 covers the mechanics):

- **Prefetch** runs up to `Grid.read_iops_max` lookups at once (`src/lsm/groove.zig:1329`). That constant is `grid_iops_read_max`, 32 by default (`src/config.zig:130`, `src/vsr/grid.zig:38`).
- **Replication.** The primary writes a prepare to its own WAL *and* sends it to every other replica in parallel (`src/vsr/replica.zig:1997–2013`).
- **Pipelining.** Up to `pipeline_prepare_queue_max = 8` prepares can be in flight (`src/config.zig:155`).
- **Compaction** reads, merges and writes blocks in a pipeline (`ARCHITECTURE.md:460–465`).

The kernel and the devices provide the parallelism. The replica's logic stays sequential.

### 3.3 How one core reaches ~1M transfers per second

The docs claim a single core "can easily scale to 1mil TPS" if you "keep it busy with useful work and move everything else off the hot path" and "do your homework: cache line aligned data structures, memory prefetching, SIMD" (`ARCHITECTURE.md:179–184`).

The budget is about 1 µs per transfer. At ~3 GHz that is roughly 3,000 cycles. A transfer's real work is two cache lookups, some compares and adds, and a few in-memory inserts. That fits, *if* nothing stalls:

| Stall | How TigerBeetle removes it | Concept |
|---|---|---|
| Lock contention | No locks: one thread | 6 |
| Disk reads inside the loop | Prefetch phase loads everything first | 10 |
| Per-request overhead (consensus, syscalls) | Paid once per batch of up to 8,189 transfers | 7 |
| Branches and dispatch per event | Homogeneous batches, `comptime` dispatch | 8 |
| Cache misses | 128-byte objects, cache-line-aware structures | 9 |
| Allocator and GC pauses | Static allocation | 11 |

I could not find a benchmark result checked into the clone that proves 1M TPS. Treat the number as TigerBeetle's stated design target, not a measurement verified here.

### 3.4 Why Zig, and why Rust's thread safety matters less here

The docs weigh Rust directly:

> "Rust has better temporal and thread safety but static allocation and single-threaded execution reduce the relative importance of these benefits." (`ARCHITECTURE.md:259–260`)

With one thread, data races cannot happen. That removes one of Rust's headline benefits from the comparison.

### 3.5 The testing payoff

One thread plus no OS time makes the replica **deterministic**: the same inputs produce the same outputs in the same order.

The simulator (VOPR, Concept 29) exploits this. It "can run an entire cluster on a single thread, injecting various storage faults and infinitely speeding up time" (`ARCHITECTURE.md:315–317`).

`Cluster.tick` (`src/testing/cluster.zig:526`) steps a simulated network and every simulated storage, then ticks each replica, all in one loop. A failure is reproduced from a seed. If replicas had internal threads, the simulator would have to control their interleaving as well, which is much harder.

The docs: "keeping the system single threaded greatly simplifies the programming model and makes testing much more efficient." (`ARCHITECTURE.md:186–187`)

## 4. What it means for ashi

### Rust features that help

- **`!Send` is free.** Types like `Rc`, `RefCell` and `Cell` are fine on one thread and cost no atomics. The compiler stops you from accidentally sharing them across threads.
- **Plain `&mut` access.** One owner of all state means the borrow checker sees simple ownership. No `Arc<Mutex<_>>`.
- **Monomorphization** gives you zero-cost specialization of the hot loop (Concept 8).

### Rust features that hurt

- **Default async runtimes are multi-threaded.** `tokio::spawn` requires `Send + 'static` futures. That pushes you toward `Arc<Mutex<_>>`.
  - If you use tokio at all, use the `current_thread` runtime with `LocalSet`, or write your own loop.
- **`Box<dyn Future>` per task** allocates and hides the concurrency limit. TigerBeetle uses explicit, statically allocated state structs instead (`ARCHITECTURE.md:213–219`; Concept 12).
- **Ecosystem crates spawn threads silently.** Examples include logging back-ends with background flush threads, some allocators' background purging, and metrics exporters. Audit every dependency.

### Patterns to use

- One `struct Replica` that owns everything, driven by `loop { replica.tick(); io.run_for(tick)?; }`.
- io_uring-based IO driven from that same thread (Concept 12). Completions dispatch to callbacks stored in the owning component.
- Pin the replica thread to a core and keep other work off that core.

### Crates worth evaluating (TigerBeetle has zero dependencies)

| Crate | What it gives | Trade-off |
|---|---|---|
| `io-uring` | Thin, low-level ring bindings | You build the event loop yourself, which is closest to TigerBeetle |
| `monoio`, `glommio`, `tokio-uring` | Thread-per-core runtimes over io_uring | Each brings futures, its own allocation patterns and its own release cadence |
| `core_affinity` | Pin a thread to a core | Tiny; easy to write by hand with `libc::sched_setaffinity` |

### Rust-specific pitfalls

- `Arc` or `Mutex` sneaking in "just in case". Make the replica type `!Send` on purpose (for example with a `PhantomData<*const ()>` field) so this fails to compile.
- Using `std::time::Instant::now()` inside logic breaks determinism. Inject time through a trait, as TigerBeetle injects ticks.
- Iterating a `std::collections::HashMap` uses a random seed per process. That makes iteration order nondeterministic across replicas and test runs. Use a fixed hasher.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt**. The workload is infinite-COST for the same reason in Rust, and a single-threaded core keeps ashi deterministic and simulator-friendly. Revisit only for work that is provably independent of ledger state (for example checksumming inbound messages), and only with measurements.

## 5. Drill questions

### Q1. Recall

What does COST stand for? What does it mean for a workload to have "infinite COST", and why does TigerBeetle claim financial transaction processing is one?

> _Your answer:_

### Q2. Mechanism

TigerBeetle is single-threaded, yet the docs say it "embraces concurrency". Explain the difference between concurrency and parallelism, and name three things that are in flight at the same time inside one TigerBeetle replica.

> _Your answer:_

### Q3. Back-of-envelope

A 4-core design shards accounts across cores. 30% of transfers touch the single `fees` account. Each cross-core handoff costs 0.1 µs, and each transfer's useful work costs 0.3 µs.

- What fraction of the work is serial?
- What is the Amdahl's-law upper bound on speedup?
- Is it worth it compared with one core doing 0.3 µs per transfer with no handoffs?

> _Your answer:_

### Q4. What breaks if…

…a contributor adds a background thread that periodically scans the accounts cache and evicts cold entries, protected by a mutex? Name at least two properties of TigerBeetle that break, beyond "it might be slower".

> _Your answer:_

### Q5. ashi design (open-ended)

Which parts of a replica could ashi move to a second core without giving up determinism? Think about inbound message checksumming, outbound message serialization, and compaction merges. For each, what would the hand-off between threads cost, and how would the simulator still control it?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** COST is the "Configuration that Outperforms a Single Thread": the number of cores a parallel system needs before it beats a competent single-threaded implementation. Infinite (unbounded) COST means no configuration ever beats the single thread. Financial transactions qualify because hot accounts (Pareto-distributed) force transfers on them to run one after another. Parallel workers then only add synchronization overhead, which "tends to dominate useful work" (`ARCHITECTURE.md:172–177`).

**Q2.** Parallelism means several cores executing instructions at the same instant. Concurrency means several operations in progress at once, possibly driven by one core. TigerBeetle has one CPU thread but many in-flight operations:
- up to 32 prefetch disk reads (`groove.zig:1329`, `grid_iops_read_max = 32`);
- a prepare being written to the local WAL while it is sent to all other replicas (`replica.zig:1997–2013`);
- up to 8 pipelined prepares (`pipeline_prepare_queue_max`);
- compaction reads and writes overlapping with merges.

The kernel and the devices provide the parallelism; the replica's logic remains sequential.

**Q3.**
- Serial fraction `s ≈ 0.3`, the transfers that touch `fees`.
- Amdahl bound: `1 / (s + (1−s)/4) = 1 / (0.3 + 0.175) ≈ 2.1×`. That is the ideal ceiling with free coordination.
- The handoffs are not free. If every `fees` transfer pays a 0.1 µs handoff, the serial path becomes 0.4 µs per hot transfer. That is 0.3 × 0.4 = 0.12 µs per transfer on average just for the serial part.
- The parallel part is 0.7 × 0.3 / 4 ≈ 0.053 µs per transfer.
- The total is about 0.17 µs, or about 1.7× faster than one core at 0.3 µs.
- But this is optimistic. It ignores deadlock avoidance, cross-shard linked chains, the lost determinism, and the tail latency of queueing on the hot shard.
- Raise the hot fraction to 60% and the bound drops to about 1.4×. The real gain likely disappears.

The point: the ceiling is low, and the costs are real.

**Q4.**
- **Determinism breaks.** Eviction timing depends on OS scheduling, so cache contents differ between replicas and between runs. Cache contents affect which prefetches hit disk and therefore IO order.
- **Prefetch guarantees break.** A value prefetched for the current batch could be evicted before `commit` reads it. `commit` assumes every prefetched object is in memory (`groove.get` reads only the cache; see Concept 10). TigerBeetle's `CacheMap` stash exists precisely to guarantee this.
- **The simulator can no longer reproduce failures from a seed.**
- **The mutex now sits on the hot path** of every cache access.
- **Static memory and IO bounds are harder to reason about.**

**Q5.** Open-ended. A strong answer:
- **Inbound checksumming is pure and per-message.** A second thread could verify a message and pass ownership through a bounded single-producer/single-consumer ring. For determinism, the replica must consume messages in an order it decides, not in completion order. The simulator can run the checker inline (synchronously) in tests.
- **Compaction merges are pure functions over blocks.** They could run on another core if the replica treats the result as an IO-like completion that it schedules deterministically. TigerBeetle already treats compaction as paced work per beat.
- **Each hand-off costs** a cross-core cache-line transfer plus queue bookkeeping. That is worth it only when the work per hand-off is large (whole messages or blocks, never single transfers).
- **The rule:** never share ledger state, and make every cross-thread result arrive as an event the single-threaded core orders itself.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
