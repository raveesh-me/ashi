# Concept 11: Static allocation, so everything has a limit

**Module:** M2 Memory & execution · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Static Memory Allocation", "io_uring" and "Zig"
- `../tigerbeetle/docs/TIGER_STYLE.md`, section "Safety" (the static-allocation rule) and "Cache Invalidation" (in-place init)
- `../tigerbeetle/docs/concepts/performance.md` and `../tigerbeetle/docs/concepts/safety.md` (the marketing-level claims)
- `../tigerbeetle/src/static_allocator.zig` (the lock), `../tigerbeetle/src/counting_allocator.zig`
- `../tigerbeetle/src/vsr/replica.zig` (`Replica.open`, `suspend_message`, `ignore_request_message`)
- `../tigerbeetle/src/tigerbeetle/main.zig` (`main`, `command_start`)
- `../tigerbeetle/src/message_pool.zig` (worst-case message count)
- `../tigerbeetle/src/stdx/stack.zig`, `../tigerbeetle/src/stdx/iops.zig`, `../tigerbeetle/src/stdx/mlock.zig`, `../tigerbeetle/src/stdx/huge_page_allocator.zig`
- `../tigerbeetle/src/vsr/journal.zig`, `../tigerbeetle/src/vsr/client_sessions.zig`, `../tigerbeetle/src/message_bus.zig`
- `../tigerbeetle/src/constants.zig`, `../tigerbeetle/src/config.zig`, `../tigerbeetle/src/tigerbeetle/cli.zig`
- `../tigerbeetle/src/clients/rust/samples/zero-allocation/src/main.rs` (TigerBeetle's own Rust take)

## 1. The problem

A ledger database runs for years. It must keep working when it is overloaded, and its latency must stay predictable, because payments time out.

Dynamic memory (`malloc`/`free`, or in Rust `Box`, `Vec::push`, `HashMap::insert`) causes three kinds of pain in that setting:

- **Unbounded growth.** Every queue, map or buffer that can grow is a place where load turns into memory. Under overload, memory keeps climbing until the out-of-memory (OOM) killer ends the process. For a replicated ledger, a crash under load is the worst time to crash.
- **Latency variance.** The allocator sometimes does expensive work: it takes a lock, asks the kernel for pages (`mmap`, page faults), or a `Vec` doubles and copies. Those spikes land in the tail (p99.9) latency, not the average.
- **Hidden failure paths.** Every allocation can fail. Either you handle OOM everywhere (nobody does it correctly), or you pretend it can't happen.

There is a fourth, subtler problem: **if nothing has a limit, nobody knows the limits.** Nobody can tell you how many concurrent disk reads, network messages or client sessions the system can hold. You only find out in production.

## 2. The obvious solution, and why it fails

**Obvious solution 1: "use a good allocator."** jemalloc or mimalloc make `malloc` fast on average. They do not bound memory, and they do not remove the tail spikes when the heap must grow. They also say nothing about how many objects the program *should* have.

**Obvious solution 2: an arena or a memory cap.** Allocate a fixed 4 GiB arena at startup and hand out memory from it. This bounds memory, but it does not reserve the *right* memory. A burst of network messages can eat the space that the journal needed for a write, and the system fails with "arena exhausted" at some random point.

A small worked example. Suppose a replica has a 2 GiB arena and each message buffer is 1 MiB (TigerBeetle's real `message_size_max`, see below). A slow peer stops reading, so 2,000 outgoing messages queue up for it. The arena is now full. The next disk read for a commit needs a buffer and fails. The bug is not "too little memory". The bug is that nothing limited the per-peer send queue.

TigerBeetle's docs make exactly this distinction (`docs/ARCHITECTURE.md`, "Static Memory Allocation"):

> Some systems allocate a fixed-sized arena at the start and fail with an 'out of memory' error if the limit is ever exceeded. That is, while the memory usage is bounded, there's no guarantee that enough memory is reserved. In TigerBeetle, the amount of memory used is a consequence of everything having an explicit upper bound.

## 3. What TigerBeetle does

### 3.1 What "static allocation" precisely is (and is not)

The docs admit the term is "somewhat idiosyncratic". Precisely:

1. At startup, for every object type, TigerBeetle computes the **worst-case upper bound** on how many objects it can need, from **compile-time constants plus runtime CLI arguments**.
2. It allocates exactly that many, from the ordinary heap.
3. It enters the event loop. After that, **no new objects are created and none are freed**.

| | Embedded "static" (`.bss`) | Arena with a cap | TigerBeetle |
|---|---|---|---|
| Where memory lives | global statics, fixed at link time | one big heap region | normal heap allocations, made once |
| Depends on CLI args? | no | the cap might | yes (cache sizes, pipeline limit, cluster size) |
| Reserves enough for each use? | yes | **no**: first-come, first-served | yes: each type gets its worst case |
| Can fail at runtime? | no | yes, "arena exhausted" | no allocation happens at runtime |

TigerStyle states the rule (`docs/TIGER_STYLE.md`, "Safety"):

> All memory must be statically allocated at startup. **No memory may be dynamically allocated (or freed and reallocated) after initialization.**

### 3.2 Worst-case bounds computed at startup

The clearest example is the **message pool**: the set of 1 MiB buffers used for every network message and WAL (write-ahead log) entry. `messages_max_replica` (`src/message_pool.zig:54`) is literally a sum of every place in the system that can hold a message at once:

```zig
sum += constants.journal_iops_read_max; // Journal reads
sum += constants.journal_iops_write_max; // Journal writes
sum += constants.client_replies_iops_read_max; // Client-reply reads
sum += constants.client_replies_iops_write_max; // Client-reply writes
sum += constants.grid_repair_reads_max;
sum += 1; // Replica.loopback_queue
sum += pipeline_limit; // Replica.Pipeline{Queue|Cache}
...
const connections_max = replica.members_count + pipeline_limit - 1;
sum += connections_max; // Connection.recv_buffer
sum += connections_max * constants.connection_send_queue_max_replica;
```

Each term names its owner in a comment. The bound is an *accounting identity*, not a guess. Some inputs are compile-time constants; others come from the CLI. `command_start` (`src/tigerbeetle/main.zig:325`) passes `members_count` (from `--addresses`) and `pipeline_requests_limit` (from `--limit-pipeline-requests`).

The real values (production config):

| Constant | Value | Where |
|---|---|---|
| `message_size_max` | `1 * MiB` | `src/config.zig:159` |
| `journal_iops_read_max` / `journal_iops_write_max` | 8 / 32 | `src/config.zig:116-117` |
| `client_replies_iops_read_max` / `_write_max` | 1 / 2 | `src/config.zig:118-119` |
| `grid_repair_reads_max` | 4 | `src/config.zig:134` |
| `pipeline_prepare_queue_max` | 8 | `src/config.zig:155` |
| `clients_max` | 64 | `src/config.zig:228` (production) |
| `connection_send_queue_max_replica` | `@max(@min(clients_max, 4), 2)` = 4 | `src/constants.zig:420` |
| default `--limit-pipeline-requests` | `div_ceil(clients_max, 2) - pipeline_prepare_queue_max` = 24 | `src/tigerbeetle/cli.zig:475` |

The pool itself is two slices plus an **intrusive free list** (`StackType(Message)`, see Concept 12). It is filled once in `init_capacity` (`src/message_pool.zig:201`). After that, `get_message` pops from the stack and `unref` pushes back when the reference count hits zero (`src/message_pool.zig:262-299`). Note what happens if the pool runs dry:

```zig
fn get_message_base(pool: *MessagePool) *Message {
    const message = pool.free_list.pop().?;   // `.?` = unwrap; null panics
```

There is no "pool exhausted" error path. Running out means the sizing arithmetic was wrong, which is a programmer bug, so it crashes (TigerStyle: assertions turn correctness bugs into liveness bugs; Concept 30).

Other bounded structures follow the same pattern:

- **Client sessions** (`src/vsr/client_sessions.zig:50-64`): a hash map sized once with `ensureTotalCapacity(allocator, clients_max)`, then filled only with `putAssumeCapacityNoClobber` / `getOrPutAssumeCapacity`. The "AssumeCapacity" variants take no allocator at all, so they *cannot* allocate.
- **Per-component IO slots**: `IOPSType(T, size)` (`src/stdx/iops.zig:6`) is a fixed array of `size` items plus a bitset. The journal has `reads: IOPSType(Read, journal_iops_read_max)` and `writes: IOPSType(Write, journal_iops_write_max)` (`src/vsr/journal.zig:250`, `:266`).
- **Caches**: grid cache from `--cache-grid` (default `grid_cache_size_default = GiB`, `src/config.zig:132`, in `block_size = 512 * KiB` blocks), account cache (default `@sizeOf(Account) * MiB` = 128 MiB, `src/config.zig:222`), LSM manifest node pool (default 8192 × 16 KiB = 128 MiB, `src/constants.zig:689`).

This is why Zig's "unmanaged" collection style matters (`docs/ARCHITECTURE.md`, "Zig"): the allocator is passed to the *methods* that allocate, not stored in the collection. Code that has no allocator in hand provably cannot allocate.

### 3.3 Locking the allocator after init

The enforcement mechanism is an 82-line wrapper, `src/static_allocator.zig`. It is a three-state machine:

```zig
const State = enum {
    /// Allow `alloc` and `resize`.
    /// (To make errdefer cleanup easier to write we also allow calling `free`,
    ///  in which case we switch state to `.deinit` and no longer allow `alloc` or `resize`.)
    init,
    /// Don't allow any calls.
    static,
    /// Allow `free` but not `alloc` and `resize`.
    deinit,
};
```

Every vtable entry asserts the state. `alloc` (`src/static_allocator.zig:58-62`) does `assert(self.state == .init)`. `free` (`:76-82`) asserts `.init or .deinit`, so even a *free* during normal operation crashes.

It is wired into the replica:

```
src/tigerbeetle/main.zig                 src/vsr/replica.zig
───────────────────────                  ───────────────────
main():                                  Replica.open():
  GeneralPurposeAllocator                  self.static_allocator =
   backed by huge_page_allocator  (:68)      StaticAllocator.init(parent)   (:664)
command_start():                           SuperBlock.init(allocator, ...)
  CountingAllocator(gpa)          (:319)   self.init(allocator, ...)  ← journal, grid,
  MessagePool.init(gpa, ...)      (:325)                                forest, caches...
  var replica: Replica = undefined (:423)  // Disable all dynamic allocation
  replica.open(gpa, ...)  ───────────────►  transition_from_init_to_static() (:739)
  log "Allocated {}MiB" (live_size) (:498) Replica.deinit():
  mlockall (best effort)          (:566)   transition_from_static_to_deinit() (:1472)
  while (true) {                  (:591)
    replica.tick();
    io.run_for_ns(tick_ms * ns_per_ms);
  }
```

Three supporting details:

- `CountingAllocator` (`src/counting_allocator.zig`) only counts bytes. It lets the operator see the total, as in "Allocated N MiB during replica init". Since nothing is allocated later, that number is the steady-state footprint.
- `huge_page_allocator` (`src/stdx/huge_page_allocator.zig:19`) is the page allocator plus `madvise(MADV_HUGEPAGE)` on Linux. It hints the kernel to back the big, long-lived regions with 2 MiB pages, which cuts TLB (address-translation cache) misses (Concept 9).
- `memory_lock_allocated` (`src/stdx/mlock.zig`) calls `mlockall(MCL_CURRENT | MCL_ONFAULT)` so the kernel never swaps the memory out. The error message explains why: swap "would otherwise bypass TigerBeetle's storage fault tolerance" (Concept 14). It is best-effort; failure only logs a warning.

TigerStyle also asks for **in-place initialization** ("Cache Invalidation" section): `var replica: Replica = undefined; replica.open(...)`. Large structs are built where they will live, so their address is stable from birth. Concept 12 relies on that stability.

> ⚠️ **Docs vs code.** `ARCHITECTURE.md` says "for every object type… TigerBeetle allocates exactly that number of objects… After startup, no new objects are created." The code *enforces* this only for allocations made through the replica's `StaticAllocator`. The `MessagePool` and the multiversion buffers are allocated in `main.zig` through the plain counting allocator, which is never locked. They are allocated once by convention, not by an assertion. The design holds; the runtime check covers the replica tree, not the whole process.

### 3.4 Limits as a forcing function

The docs' summary: "**static allocation is a forcing function to ensure that everything has a limit**." You cannot write `messages_max_replica` without answering: how many reads can the journal have in flight? How many connections can exist? How deep is each send queue? Each answer becomes a named constant with a comptime assertion. The docs say this "simplifies the system greatly. You spend more time thinking up-front but after the initial design the interplay tends to just work out."

TigerStyle's broader rule is the same idea applied to time: "**Put a limit on everything**… all loops and all queues must have a fixed upper bound."

### 3.5 Emergent backpressure

Backpressure means slowing the sender when the receiver is full. Most systems bolt it on: rate limiters, queue-depth checks, 503 responses. TigerBeetle has **no dedicated backpressure module**. It falls out of the limits (`docs/ARCHITECTURE.md`): "If everything has a limit, there's nothing to grow without bound to begin with."

Three real places where a full limit turns into backpressure:

```
client ──request──► primary
                     │ pipeline full? (prepare queue 8 + request queue)
                     │   └─► ignore request; client times out and retries      (replica.zig:6777)
                     ▼
peer ──prepare────► replica
                     │ journal.writes.available() == 0 ?
                     │   └─► suspend_message: leave it in the recv buffer       (replica.zig:1692)
                     │         └─► bus stops calling recv() on that connection  (message_bus.zig:899)
                     │               └─► kernel TCP window fills → sender slows
                     ▼
               journal write (IOPSType slot)
```

The second chain is the elegant one. The replica doesn't drop the prepare or queue it in a growing list. It simply doesn't read more bytes from that socket until a journal write slot frees up. TCP's own flow control then pushes back on the sender. The limit on 32 in-flight journal writes propagates across the network with no extra code.

Clients are bounded too: each client has one request in flight (Concept 27), so the client count `clients_max = 64` bounds the request load the primary must buffer.

### 3.6 Latency variance

The docs list performance last, on purpose: "Static allocation makes the system faster and greatly reduces latency variation… but it is not the main reason." Why less variance:

- No allocator calls on the hot path, so no allocator locks and no `mmap` calls to grow the heap. (Whether every page is also *touched* at startup, so no first-touch page fault ever happens later, I could not verify from the source.)
- No collection ever grows, so there is no "every 2^k-th insert copies everything" spike.
- No fragmentation over months of uptime. The memory layout on day 300 is the same as on day 1.

The deeper reason is the theme of Concepts 6–10: the system's "physics" are known. The maximum work per tick is bounded because every queue it drains is bounded.

## 4. What it means for ashi

### Rust features that help

- **`Vec::with_capacity(n)`** guarantees no reallocation while `len < capacity`. A `Box<[T]>` (from `vec.into_boxed_slice()`) goes further: it has no capacity and cannot grow at all. Use boxed slices for every fixed pool.
- **Fixed-capacity containers**: `[T; N]` with const generics, `arrayvec::ArrayVec<T, N>` (`try_push` returns `Err` when full), `heapless::Vec`/`heapless::IndexMap` (built for no-allocation embedded use).
- **`#[global_allocator]`** lets you replace the process allocator and so build TigerBeetle's lock for the *whole process*, which is stronger than TigerBeetle's per-replica lock:

```rust
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

const INIT: u8 = 0; const STATIC: u8 = 1; const DEINIT: u8 = 2;
pub static STATE: AtomicU8 = AtomicU8::new(INIT);

struct StaticAlloc;
unsafe impl GlobalAlloc for StaticAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if STATE.load(Relaxed) != INIT { std::process::abort(); } // never unwind here
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if STATE.load(Relaxed) == STATIC { std::process::abort(); }
        System.dealloc(p, l)
    }
    // realloc/alloc_zeroed: guard them too (the defaults route through alloc).
}
#[global_allocator] static A: StaticAlloc = StaticAlloc;
```

  `GlobalAlloc` must not unwind (the std docs say unwinding out of a global allocator is undefined behavior), so use `abort()`, not `panic!`. Track bytes in the same wrapper to log the footprint, like `CountingAllocator`.

### Rust features that hurt

- **std collections are allocator-coupled.** `Vec`, `HashMap` and `String` hide the allocator. Nothing in the type stops `push` past capacity from silently reallocating. Zig's `putAssumeCapacity` has no allocator to call; Rust has no stable equivalent. `Vec::push_within_capacity` exists but is unstable (nightly-only) at the time of writing. On stable, write a thin wrapper: `assert!(v.len() < v.capacity()); v.push(x)`.
- **`HashMap::with_capacity(n)`** promises room for `n` elements. Under heavy insert/remove churn, hashbrown's tombstone handling can trigger a rehash. Do not assume it never allocates. Prove it with the locked allocator under a churn test.
- **The per-collection allocator API** (`Vec::new_in`, the `Allocator` trait) is unstable. The `allocator-api2` crate mirrors it on stable, and `hashbrown` can use it.
- **Implicit allocation is everywhere:** `format!`, `to_string()`, `Box<dyn Error>`, `anyhow`, error `Display` paths, `std::thread::spawn`, lazily initialized `stdout` buffers, panic messages, backtraces. A locked global allocator will find all of them, usually at an awkward moment. That's the point: find them in tests, not in production.
- **Moves.** Rust moves values by default, and `Box::new(BigStruct { .. })` may build the struct on the stack first and then copy it. For multi-MiB structs, use `Box::<T>::new_uninit()` and write fields in place (TigerStyle's in-place init), or allocate a zeroed slab.
- **Overcommit.** `vec![0u8; n]` uses zeroed allocation, which Linux may back lazily. "Allocated" is not "resident". If you want TigerBeetle's "memory is reserved" property, touch every page during init and `mlockall` (via `libc` or `rustix`).

### Patterns to use

- One `Config` struct computed from CLI args → one pure function per pool: `fn messages_max(cfg) -> u32`, written as a commented sum like `messages_max_replica`, plus `const` assertions on the compile-time parts.
- Pools as `Box<[Slot]>` + an index free list (`u32` indices, not pointers). Indices dodge most `unsafe` and borrow-checker friction and keep slots `Copy`-friendly.
- Typed handles (`struct MsgId(u32)`) with reference counts, like `Message.references`.
- Single-writer core (Concept 6), so pools need no `Mutex` or atomics.
- Return "slot unavailable" as a *normal* state that suspends work (as `suspend_message` does), not as an error.

### Crates worth evaluating

TigerBeetle has zero dependencies (Concept 13), so every crate below is a trade-off, not a default.

| Crate | Use | Trade-off |
|---|---|---|
| `assert_no_alloc` | traps allocation inside a closure, via a custom global allocator | per-thread scope; good for tests of the hot loop |
| `dhat`, `stats_alloc` | count allocations in tests and assert on them | test-only; no runtime cost in release if feature-gated |
| `heapless`, `arrayvec` | fixed-capacity `Vec`/map/string | capacity is a const generic, so it can't come from CLI args |
| `slab` | index-addressed pool | **grows** when full; check `len() < capacity()` before insert |
| `bumpalo` | arena | an arena is exactly what the docs say TigerBeetle is *not*; fine for init-time scratch |
| `allocator-api2` | stable allocator trait for `hashbrown` | extra API surface; the std version may change it |

The real TigerBeetle team faced this in Rust. Their Rust client offers `*_reusable` methods that take a reusable `Completion` plus owned `Vec`s and give them back. The sample `src/clients/rust/samples/zero-allocation/src/main.rs` preallocates with `Vec::with_capacity(batch_size_max)` and then loops with no heap allocation per request.

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt**. Rust can enforce this *more* strongly than TigerBeetle does (a process-wide `#[global_allocator]` lock), at the price of wrapping std collections.

## 5. Drill questions

### Q1. Recall

Name the three states of `StaticAllocator` and say which allocator calls each one permits. Why is `free` allowed in `.init`?

> _Your answer:_

### Q2. Mechanism

A teammate says: "Static allocation is just an arena with a 2 GiB cap." Explain precisely why TigerBeetle's approach is different, and why the difference matters under overload.

> _Your answer:_

### Q3. Back-of-envelope

A 3-replica production cluster runs with default flags. Using `messages_max_replica` in `src/message_pool.zig:54` and the constants table in §3.2:

- How many messages does each replica's pool hold?
- How much memory do their buffers take?

> _Your answer:_

### Q4. What breaks if…

Someone adds a new feature that keeps one extra message per connected client while waiting for an ack, but forgets to add a term to `messages_max_replica`.

- What happens, and when?
- Why is that better than the same bug in a `malloc`-based system?
- What in TigerBeetle's process would most likely catch it before release?

> _Your answer:_

### Q5. ashi design (open-ended)

ashi will be Rust with a `#[global_allocator]` lock. List three places where std or common crates would allocate behind your back in a replica's hot loop. For each, give your plan: wrap, replace, or move to init. Then decide: do you lock *deallocation* too, as TigerBeetle does, and how do you handle shutdown?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- `.init` allows `alloc`, `resize` and `remap`.
- `.static` allows nothing. Any call asserts.
- `.deinit` allows only `free`.

`free` is allowed in `.init` so `errdefer` cleanup works when initialization fails halfway. The first `free` flips the state to `.deinit` ("Once you start freeing, you don't stop."). `transition_from_init_to_static` is called at `replica.zig:739`; `transition_from_static_to_deinit` is called at `replica.zig:1472`.

**Q2.** An arena bounds the *total* but not *each use*. Memory goes to whoever asks first, so a burst in one subsystem (say, send queues to a slow peer) can starve another (a journal write) and fail at a random point.

TigerBeetle computes a worst case *per object type* and reserves each one. The total is a consequence of the limits, not a cap on top of them. Under overload, a full limit means "wait", not "fail": the request is ignored, the message stays in the socket buffer, the slot is busy. So the system degrades into backpressure instead of crashing.

**Q3.** `members_count = 3`; `pipeline_limit = 8 + 24 = 32`.

| Term | Count |
|---|---|
| journal reads | 8 |
| journal writes | 32 |
| client-reply reads / writes | 1 + 2 |
| grid repair reads | 4 |
| loopback | 1 |
| pipeline | 32 |
| commit_prepare + sync_view | 1 + 1 |
| join-view quorum: `min(3, 6)` | 3 |
| burst + commit reply | 1 + 1 |
| **subtotal** | **87** |

- TCP: `connections_max = 3 + 32 − 1 = 34`.
- Receive buffers: 34.
- Send queues: `34 × 4 = 136`.
- **Total: 87 + 34 + 136 = 257 messages.**

At `message_size_max = 1 MiB` that is **257 MiB** of message buffers, plus the small `Message` headers. For scale, the defaults also reserve 1 GiB of grid cache, 128 MiB of account cache and 128 MiB of manifest nodes.

**Q4.**
- Under the right load (many clients, each holding an extra message), the free list empties. `pool.free_list.pop().?` unwraps null and the replica panics.
- That's better than `malloc` because the failure is **deterministic and loud**: it happens at an exact, reproducible count, not as gradual memory growth followed by an OOM kill hours later, or an allocation failure in some unrelated subsystem.
- The VOPR (Concept 29) would most likely catch it before release. It picks a random client count from 1 up to `clients_max * 2 - 1` (`src/vopr.zig:386`), so it routinely runs with more clients than the server accepts, under injected faults, and the panic reproduces from the seed. The pattern of naming each term's owner in a comment also makes the omission visible in code review.

**Q5.** Example hidden allocations:
- `format!`/`to_string` in logging and errors. **Plan:** preallocated log ring buffer, `write!` into a fixed `[u8; N]` buffer.
- `Box<dyn Error>`/`anyhow` on error paths. **Plan:** plain `enum` errors.
- `HashMap` rehash under churn. **Plan:** your own open-addressing table over a `Box<[Slot]>`, or prove `hashbrown` under a churn test.
- `Vec::push` past capacity. **Plan:** a `push_within_capacity` wrapper that asserts.
- Thread spawn, `std::env`, backtrace capture on panic. **Plan:** move to init, or accept them because a panic aborts anyway.
- Async executors' `spawn` (a heap task per spawn; see Concept 12). **Plan:** don't spawn after init.

Locking dealloc: yes, mirroring TigerBeetle, because a stray `drop` of a `String` during operation means something *was* allocated dynamically or a pool object was mis-owned. Shutdown: add a `DEINIT` state that allows only frees, or skip teardown entirely and `exit` (the OS reclaims memory). TigerBeetle has a full `deinit` path, and `main` checks the `GeneralPurposeAllocator` for leaks on exit (`src/tigerbeetle/main.zig:70-76`).

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
