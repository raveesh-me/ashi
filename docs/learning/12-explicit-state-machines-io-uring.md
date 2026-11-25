# Concept 12: Explicit state machines instead of async futures; io_uring; intrusive lists

**Module:** M2 Memory & execution · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Static Memory Allocation" (runaway concurrency), "Synchronous Execution", "Embracing Concurrency" and "io_uring"
- `../tigerbeetle/docs/TIGER_STYLE.md`, "Naming Things" (callback naming) and "Cache Invalidation" (in-place init, pointer stability)
- `../tigerbeetle/docs/internals/HACKING.md` (kernel requirement)
- `../tigerbeetle/src/stdx/io.zig` (backend selection), `../tigerbeetle/src/stdx/io/linux.zig` (io_uring backend)
- `../tigerbeetle/src/stdx/io/darwin.zig`, `../tigerbeetle/src/stdx/io/windows.zig`, `../tigerbeetle/src/stdx/io/linux_epoll.zig`, `../tigerbeetle/src/stdx/io/io_testing.zig`
- `../tigerbeetle/src/stdx/queue.zig`, `../tigerbeetle/src/stdx/stack.zig` (intrusive lists), `../tigerbeetle/src/stdx/iops.zig`
- `../tigerbeetle/src/storage.zig`, `../tigerbeetle/src/vsr/journal.zig`, `../tigerbeetle/src/vsr/grid.zig`
- `../tigerbeetle/src/vsr/replica.zig` (`CommitStage`, `commit_dispatch`)
- `../tigerbeetle/src/testing/storage.zig` (simulated storage)
- `../tigerbeetle/src/constants.zig`, `../tigerbeetle/src/config.zig`, `../tigerbeetle/src/tigerbeetle/main.zig`
- `../tigerbeetle/src/clients/rust/src/lib.rs`, `../tigerbeetle/src/clients/rust/samples/zero-allocation/src/main.rs`

## 1. The problem

TigerBeetle runs on one thread (Concept 6). Yet at any moment it must have many operations in flight at once:

- prefetch reads for the next batch, issued in parallel (Concept 10);
- WAL (write-ahead log) writes for several prepares;
- sends and receives on every peer and client connection;
- LSM compaction reads and writes, pipelined.

A blocking syscall would stall all of that while one disk read completes. So the single thread needs **asynchronous IO**: start an operation, go do other work, and get told later when it finished.

Every asynchronous operation needs a **resumption context**: the memory that says "when this read finishes, here is the buffer, here is the offset, and here is what to do next". The real design question is: **where does that memory live, and who bounds how many exist?** Concept 11 already forbids allocating it on the fly.

## 2. The obvious solution, and why it fails

**Obvious solution 1: threads.** Use a thread pool for blocking disk IO and a thread per connection. This brings back everything Concept 6 removed: locks, context switches, and nondeterministic scheduling. Nondeterminism alone kills deterministic simulation (Concept 29): a test failure would not reproduce from a seed.

**Obvious solution 2: epoll.** Readiness-based APIs (`epoll`, `kqueue`) tell you when a socket is *ready*, and then you do the syscall yourself. That works for sockets. For regular files, epoll does not give you asynchronous disk reads: a file always reports "ready" and the `pread` still blocks. The docs: io_uring "is _also_ the only reasonable way to have truly asynchronous disk io on Linux".

**Obvious solution 3: async/await with an executor.** This is the mainstream answer in Rust (tokio) and many other languages. Each concurrent task is a future, typically spawned onto an executor that **heap-allocates** it (the docs' "`Box<dyn Future>`"). Two problems follow:

- **An allocation per task.** At 100,000 requests/second, with a few tasks per request, that's hundreds of thousands of allocations per second, in direct conflict with Concept 11.
- **Runaway concurrency.** `spawn` has no natural limit. A burst of 50,000 connections can create 50,000 tasks, each holding buffers. The docs call this oversubscription: "more concurrent tasks than the number of underlying resources available."

## 3. What TigerBeetle does

### 3.1 Completion-based IO and io_uring

There are two IO models:

- **Readiness** (epoll): "tell me when I *can* read; then I'll call `read`."
- **Completion** (io_uring, Windows IOCP): "here is a read, with buffer and offset; tell me when it is *done*."

io_uring is a pair of ring buffers shared between the process and the kernel:

```
          user space (TigerBeetle)                    kernel
 ┌──────────────────────────────────────┐
 │ SQ  submission queue (128 entries)   │──io_uring_enter()──►  executes ops
 │ [sqe: op=read fd off buf user_data]  │                      (disk, network)
 └──────────────────────────────────────┘                            │
 ┌──────────────────────────────────────┐                            │
 │ CQ  completion queue                 │◄───────────────────────────┘
 │ [cqe: user_data, res]                │   res = bytes or -errno
 └──────────────────────────────────────┘
```

- An **SQE** (submission queue entry) describes one operation.
- A **CQE** (completion queue entry) reports its result.
- `user_data` is a 64-bit value that the kernel copies, untouched, from the SQE to the CQE. TigerBeetle puts a **pointer to the operation's `Completion` struct** there.

Batching is built in (Concept 7). Many SQEs go to the kernel in one `io_uring_enter` syscall, and many CQEs come back in one copy. The docs say this interface is "a perfect fit": it combines batching and fine-grained concurrency, "more or less exactly" how TigerBeetle wants to issue IO.

TigerBeetle's Linux backend uses Zig std's `linux.IoUring` binding (`src/stdx/io/linux.zig:5`). The ring is created in `main` with 128 entries for `start` (2048 for `format`/`recover`, which issue many writes at once) (`src/tigerbeetle/main.zig:104-110`). `IO.init` refuses to run without `IORING_FEAT_EXT_ARG` (`src/stdx/io/linux.zig:69`), which lets it pass a wait timeout directly to `io_uring_enter`.

> ⚠️ **Docs vs code.** `docs/internals/HACKING.md` says TigerBeetle "requires a fairly modern kernel (≥ 5.6)". The server's io_uring backend panics with "Linux kernel 5.11 or greater is required for IORING_ENTER_EXT_ARG" (`src/stdx/io/linux.zig:69-71`). The code wins: the **server** needs ≥ 5.11. (Linux *clients* use an epoll backend instead; see §3.7.)

### 3.2 The `Completion` struct and the event loop

Every asynchronous operation is a `Completion` (`src/stdx/io/linux.zig:377`):

```zig
pub const Completion = struct {
    io: *IO,
    result: i32 = undefined,
    link: QueueType(Completion).Link = .{},   // intrusive list hook (§3.5)
    operation: Operation,                     // tagged union: read/write/send/recv/accept/...
    context: *anyopaque,                      // the owner, type-erased
    callback: *const fn (context: *anyopaque, completion: *Completion,
                         result: *const anyopaque) void,
};
```

The caller supplies the `Completion`, and the caller owns its memory. `IO.read` (`src/stdx/io/linux.zig:1050`) only fills it in and queues it:

```zig
pub fn read(self: *IO, comptime Context: type, context: Context,
    comptime callback: fn (context: Context, completion: *Completion,
                           result: ReadError!usize) void,
    completion: *Completion, fd: fd_t, buffer: []u8, offset: u64) void
```

The callback is **typed** at the call site. `erase_types` (`:1972`) generates a small comptime wrapper that casts back from `*anyopaque`. The struct stores an erased pointer, but the user never writes a cast.

The life of one read:

```
component            IO (linux.zig)                       kernel
─────────            ──────────────                       ──────
io.read(..., &c) ─► enqueue(c)           :214
                      sqe = ring.get_sqe()  (SQ full? flush to kernel, retry)
                      c.prep(sqe); sqe.user_data = @intFromPtr(c)   :462
main loop:
io.run_for_ns(10ms) ► flush_submissions ─────io_uring_enter────► does the read
                      flush_completions  ◄────── cqes[256] ──────
                        c = @ptrFromInt(cqe.user_data)          :190
                        c.result = cqe.res
                        completed.push(c)  (intrusive queue)    :197
                      run_callback: c.complete()
                        errno → typed Zig error; callback(ctx, c, result)
```

Two deliberate details:

- **Callbacks never run inside `flush_completions`.** CQEs are first moved to the `completed` queue. The code comment (`:193-196`) lists the reasons: to avoid "recursion through `flush_submissions()` and `flush_completions()`", "unbounded stack usage", and "confusing stack traces".
- **`next_tick`** (`:340`) pushes a `Completion` straight onto `completed` without touching the kernel. Components use it to defer a callback to the next loop iteration instead of calling it synchronously. Stack depth stays bounded, and a callback never re-enters its caller.

The main loop is just (`src/tigerbeetle/main.zig:591-595`):

```zig
while (true) {
    replica.tick();
    try io.run_for_ns(constants.tick_ms * std.time.ns_per_ms);  // tick_ms = 10
    emit_stats(tracer, &io.stats);
}
```

### 3.3 Components own their completions

The docs: "each component stores its own resumption contexts. For example, the `Journal` holds a fixed-size array of `Write` structures with callback contexts." Contexts nest, each layer embedding the one below **by value**:

```
Journal.Read  (journal.zig:148)        one of IOPSType(Read, journal_iops_read_max = 8)
├── journal: *Journal
├── message: *Message.Prepare
├── callback: Callback                  ← what the replica wants next
└── completion: Storage.Read  (storage.zig:22)
    ├── buffer, offset, zone, callback
    └── completion: IO.Completion        ← what the kernel's user_data points at
```

When the read finishes, each layer climbs back up with `@fieldParentPtr`, Zig's built-in `container_of`. It turns "pointer to field X" into "pointer to the struct that contains X":

```zig
fn on_read(self: *Storage, completion: *IO.Completion, ...) void {
    const read: *Storage.Read = @fieldParentPtr("completion", completion);  // storage.zig:252
...
fn read_prepare_with_op_and_checksum_callback(completion: *Storage.Read) void {
    const read: *Journal.Read = @alignCast(@fieldParentPtr("completion", completion)); // journal.zig:833
    const journal = read.journal;
    const replica: *Replica = @alignCast(@fieldParentPtr("journal", journal));       // :835
```

There are no maps from request ID to state and no heap closures. The address of the field *is* the identity of the operation. That only works because components never move once they are initialized (TigerStyle's in-place init and pointer stability; Concept 11).

### 3.4 Explicit state machines instead of coroutines

Multi-step flows are written as **explicit state machines**: an enum of stages plus a dispatcher function that resumes from the current stage. The best example says so in its own comment (`src/vsr/replica.zig:4376-4392`):

```zig
/// This is a manual desugaring of asynchronous function of the following shape:
///
/// loop {
///     prefetch().await;
///     execute().await;
///     compact().await;
/// }
///
/// - commit_dispatch_enter starts the loop.
/// - for asynchronous operations, return from the loop and arrange for
///   commit_dispatch_resume to be called when IO is done.
```

The state is `commit_stage: CommitStage` (`replica.zig:452`, enum at `:83`). The dispatcher walks the stages in this order: `idle → start → check_prepare → prefetch → stall → reply_setup → execute → checkpoint_durable → compact → checkpoint_data → checkpoint_superblock`. Each step returns `.pending` (an IO is in flight; return to the event loop) or `.ready` (continue in the same loop iteration). The loop has a safety bound, `for (0..constants.vsr_checkpoint_ops)`, so even a fully synchronous run of commits is bounded.

Why hand-write what a compiler could generate?

- **No hidden allocation.** The whole "coroutine frame" is a few fields on `Replica`, sized at compile time.
- **The state is data.** You can assert on it (`assert(self.commit_stage == .idle)`), log it, trace it per stage, and switch on it exhaustively.
- **Cancellation is explicit.** `commit_dispatch_cancel` (`:4577`) unrefs the prepare and resets the stage. The comment above `commit_dispatch` spells out the order: wait for writes, stop the loop, cancel in-flight grid reads, then reset. With futures, cancellation is "drop the future", which is exactly when completion-based IO gets dangerous (see §4).
- **Fine-grained concurrency is natural.** The docs: "the natural way to write code is to issue two concurrent syscalls" (for example, reading two LSM levels at once). With callbacks that is just two `io.read` calls.

### 3.5 Intrusive linked lists

An **intrusive** list stores the `next` pointer *inside* the element (a `link` field), instead of allocating a separate list node that points to the element. Inserting therefore never allocates. An element can sit in at most one list per link field.

```
non-intrusive (std::list / LinkedList<T>):    intrusive (stdx.QueueType):
 [node]──►[node]──►[node]     ← allocated      ┌Completion────┐  ┌Completion────┐
   │        │        │          per push       │ ...          │  │ ...          │
   ▼        ▼        ▼                         │ link.next ───┼─►│ link.next=null│
  elem     elem     elem                       └──────────────┘  └──────────────┘
```

TigerBeetle has two, in `src/stdx`:

- `QueueType(T)` (`src/stdx/queue.zig:10`): FIFO with `in`/`out` pointers, `push`, `push_first`, `pop`, `remove` (O(n)), and an iterator. It has **no capacity**: its length is bounded only by how many `T`s exist.
- `StackType(T)` (`src/stdx/stack.zig:11`): LIFO with an explicit `capacity` and `assert(self.count < self.capacity)` on push.

Both convert from link back to element with `@fieldParentPtr("link", link)`. Both have a non-generic `*Any` core "for smaller binary and faster compile times". Both can `verify_push`: an O(n) assertion that the element isn't already in the list, which is the classic intrusive-list bug.

Where they're used:

| List | Holds | Where |
|---|---|---|
| `IO.completed` | completions ready for their callback, plus `next_tick`s | `src/stdx/io/linux.zig:36` |
| `MessagePool.free_list` | unused messages | `src/message_pool.zig:188` |
| `Grid.write_queue`, `read_queue`, `read_pending_queue` | grid IOs waiting for a free IOP (IO operation slot) | `src/vsr/grid.zig:213-236` |
| `MessageBus.connections_suspended` | connections paused for backpressure (Concept 11) | `src/message_bus.zig:59` |

The key trick is in the grid. `write_iops` is a fixed pool of 32 slots (`grid_iops_write_max`). When all 32 are busy, a caller's `Write` (which the *caller* owns) goes into the intrusive `write_queue`. The queue needs no storage of its own, so "wait for a slot" costs zero allocation, however many callers wait.

> ⚠️ **Docs vs code (minor).** `src/stdx/stack.zig:10` says the element "must have a field called "next"", but the code uses `@fieldParentPtr("link", ...)` (`:44`). The field is `link`.

### 3.6 The implicit global concurrency limit

The docs: "`IO` can manage arbitrary many in-flight IO operations without a hard-coded upper bound. And yet, the total amount of concurrency is limited. The limit is implicit, it is the sum of per-component explicit limits."

For storage, `src/constants.zig:510-517` writes that sum down:

```zig
pub const iops_read_max = journal_iops_read_max + client_replies_iops_read_max +
    grid_iops_read_max + superblock_iops_read_max;   // 8 + 1 + 32 + 1 = 42
pub const iops_write_max = journal_iops_write_max + client_replies_iops_write_max +
    grid_iops_write_max + superblock_iops_write_max; // 32 + 2 + 32 + 1 = 67
```

The simulator *checks* it. The simulated `Storage` keeps pending IOs in priority queues sized to exactly `iops_read_max` and `iops_write_max` (`src/testing/storage.zig:234-240`), and `PriorityQueue.push` asserts `items.len < capacity` (`src/stdx/priority_queue.zig:32`). A component that exceeds its budget crashes the VOPR (Concept 29).

The io_uring ring (128 SQEs) is *not* the limit. If the SQ fills, `enqueue` logs a warning, flushes to the kernel and retries (`linux.zig:214-231`). The limit lives in the components; the ring is just a conveyor.

> ⚠️ **Docs vs code.** `ARCHITECTURE.md` says "The contexts from different components are organized by `IO` into a single intrusive linked list." In the io_uring backend, *in-flight* operations are in no list at all: the kernel holds them and returns the `Completion` pointer via `user_data`. Only *finished* operations (and `next_tick`s) go into the intrusive `completed` queue. The Darwin backend does keep `io_pending` and `timeouts` queues (`darwin.zig:32-40`), because it has to retry operations itself.

### 3.7 Other OSes and simulation

`src/stdx/io.zig:11-23` picks a backend at compile time. All share the same callback-plus-`Completion` API:

| Backend | Mechanism | Notes |
|---|---|---|
| `linux.zig` | io_uring | production server |
| `linux_epoll.zig` | epoll | Linux **clients** ("for compatibility with older kernels and restricted containers") |
| `darwin.zig` | kqueue | file `pread`/`pwrite` are **synchronous** (`darwin.zig:845-848`); development only |
| `windows.zig` | IOCP (Windows' completion API) | development |
| `io_testing.zig` | in-memory files | "only implements what is needed to test Storage" |

The Darwin backend shows that a completion API can be *emulated* on top of readiness. It runs the syscall itself, re-queues on `WouldBlock`, and then invokes the same callback.

For the full-cluster simulator, TigerBeetle swaps out a *higher* layer instead of `IO`. The replica is generic over its environment, `ReplicaType(StateMachine, MessageBus, Storage, AOF)` (`src/vsr/replica.zig:144`). The VOPR passes `src/testing/storage.zig` ("In-memory storage, with simulated faults and latency") and a simulated message bus. Because components only ever see "start op, get callback later", the same replica code runs against real io_uring or against a seeded, fault-injecting simulator. This is the deep payoff of callbacks: **the scheduler is data you control**, not a runtime you link.

> ⚠️ **Docs vs code.** `ARCHITECTURE.md` says "TigerBeetle uses io_uring exclusively for IO". That holds for the production server on Linux. The codebase also ships epoll (Linux clients), kqueue and IOCP backends.

## 4. What it means for ashi

### The real Rust conflict (it is not "async is slow")

A Rust `async fn` already compiles to an explicit state machine, an enum much like `CommitStage`, with no allocation by itself. The conflict with TigerBeetle's design comes from around it:

1. **Executors allocate per task.** `tokio::spawn`, `monoio::spawn` and `glommio` tasks are heap-allocated, and nothing bounds how many you spawn. That breaks Concept 11.
2. **Cancellation by `drop` vs completion IO.** If a future that issued an io_uring read is dropped, the kernel *still writes into the buffer*. So the buffer can't be a borrowed `&mut [u8]`; the operation must own it. That is why `tokio-uring` and `monoio` use **owned buffers** (`read_at(buf, pos)` returns `(result, buf)`). TigerBeetle's own Rust client hit the same wall. Its `*_reusable` methods take `Vec`s by value and return them, and the sample cites "the kernel must own the buffer" (`src/clients/rust/samples/zero-allocation/src/main.rs:41-47`).
3. **Determinism.** Multi-threaded work-stealing runtimes schedule nondeterministically, which breaks simulation (Concept 29).
4. **`dyn` futures need `Box`.** Recursion and trait objects returning futures need `Box::pin`. (Async fn in traits is stable since Rust 1.75, but only for static dispatch.)

### Options for ashi

| Option | Fits static allocation? | Deterministic sim? | Cost |
|---|---|---|---|
| Hand-written state machines + callbacks over the raw `io-uring` crate (TigerBeetle's way) | yes | yes, if IO is a trait you can swap | verbose; `unsafe` at the SQ push (`push` is `unsafe` because buffers must outlive the op) |
| `async fn` + **your own** single-threaded executor with fixed task slots | yes, if task count is fixed at init | yes | build the executor and waker; future sizes need care |
| `monoio` | per-task allocation | not built in | thread-per-core io_uring (epoll/kqueue fallback), owned buffers, `!Send` tasks |
| `glommio` | per-task allocation | not built in | Linux/io_uring only, thread-per-core; check maintenance activity before adopting |
| `tokio-uring` | per-task allocation | no | owned-buffer API; development has been slow |
| `compio` | per-task allocation | not built in | completion-based across io_uring/IOCP, with a polling fallback |
| `tokio` (epoll) | no | no | `tokio::fs` runs blocking file IO on a thread pool, the opposite of this design |

Embedded Rust has a precedent for bounded futures: `embassy-executor` declares tasks with `#[task(pool_size = N)]` and stores them statically. Spawning beyond `N` fails instead of allocating.

### A Rust-friendly adaptation: indices instead of pointers

TigerBeetle puts a raw pointer in `user_data` and climbs with `@fieldParentPtr`. In Rust that needs `unsafe`, `Pin` and careful aliasing. A safer equivalent with the same memory behavior:

```rust
#[repr(u8)] enum Owner { Journal, Grid, Bus }      // which component
// user_data = (owner as u64) << 56 | (slot as u64)  // index into that component's slots

match decode(cqe.user_data()) {
    (Owner::Journal, slot) => journal.on_complete(slot, cqe.result()),
    (Owner::Grid, slot)    => grid.on_complete(slot, cqe.result()),
    ...
}
```

Each component keeps `slots: Box<[Slot; N]>` plus a bitset, which is exactly `IOPSType`. Dispatch is a `match`: static, exhaustive, and with no function pointers or type erasure. Intrusive lists can also be **index-based**: `next: Option<u32>` inside each slot. You keep O(1) and zero allocation, and nothing can dangle across a move.

### Pinning and intrusive lists in Rust, if you go pointer-based

- Rust moves values freely. A node in an intrusive list must not move while linked. `Pin<&mut T>` plus a `PhantomPinned` marker makes moving impossible in safe code. In practice, allocate components once at init (`Box`, or `Box::leak` for a `&'static`) and never move them: TigerStyle's in-place init, in Rust form.
- **Aliasing.** A component holding `&mut self` while the list also holds a pointer into it is undefined behavior under Rust's aliasing rules. Keep links in `Cell`/`UnsafeCell` and go through raw pointers. Run the list logic under Miri, with a simulated IO backend (Miri can't run real io_uring syscalls).
- `core::mem::offset_of!` (stable since Rust 1.77) gives the field offset you need for a `container_of`.
- Crates: `intrusive-collections` (adapters via `intrusive_adapter!`; supports `&'a` and `UnsafeRef` links, which suit statically allocated nodes) and `cordyceps` (no-std intrusive structures). Tokio itself uses an internal intrusive list for its waiter queues.

### Pitfalls specific to Rust

- Reusing or dropping a slot while its SQE is in flight is a use-after-free that the borrow checker cannot see: the kernel is the other "borrower". Track per-slot state (`Free → Submitted → Completed`) and assert it, like `IOPSType.release` asserting `busy.is_set(i)`.
- Don't run callbacks inside the CQE drain loop; copy to a ready queue first (TigerBeetle's reasons in §3.2 apply unchanged).
- Large compiler-generated futures can blow the stack if built on it. Check `size_of_val(&fut)`.

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adapt**. Keep the completion-based, component-owned, bounded design, but use index-based `user_data` and index-linked lists, so most of the core stays in safe Rust.

## 5. Drill questions

### Q1. Recall

What does TigerBeetle store in an io_uring SQE's `user_data`? Walk through how a CQE turns back into a call to `Journal`'s callback. Name each `@fieldParentPtr` hop.

> _Your answer:_

### Q2. Mechanism

Explain why `flush_completions` pushes completions onto `IO.completed` instead of invoking callbacks immediately. Give a concrete bad scenario the direct approach would cause.

> _Your answer:_

### Q3. Back-of-envelope

Using `src/constants.zig:510-517` and `src/config.zig:116-131`:

- What is the maximum number of concurrent *storage* operations (reads + writes) a replica can have in flight?
- Is the 128-entry submission ring a problem? Why or why not?
- What else, besides storage, shares that ring?

> _Your answer:_

### Q4. What breaks if…

A refactor changes `Journal` so that its `reads` array lives in a `std.ArrayList` that can grow by reallocating. Reads are in flight when it grows. What exactly breaks, and in which layer would you first notice it?

> _Your answer:_

### Q5. ashi design (open-ended)

Choose for ashi:

- (a) hand-written state machines over the `io-uring` crate, with index-based `user_data`; or
- (b) `async fn` on a custom single-threaded executor with a fixed number of task slots.

For your choice, explain how you would: bound concurrency, handle cancellation of an in-flight read during state sync, and run the same code under a deterministic simulator.

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** `user_data = @intFromPtr(completion)`: the address of an `IO.Completion` (`linux.zig:462`). On completion:

1. `flush_completions` does `@ptrFromInt(cqe.user_data)`, sets `result`, and pushes it onto `completed`.
2. `run_callback` → `complete()` maps errno to a typed error and calls the erased callback, `Storage.on_read`.
3. Hop 1: `@fieldParentPtr("completion", completion)` gives the `Storage.Read` (`storage.zig:252`). Storage may retry partial reads; when done, it calls `read.callback`.
4. Hop 2: `@fieldParentPtr("completion", ...)` gives the `Journal.Read` (`journal.zig:833`).
5. Hop 3: `@fieldParentPtr("journal", journal)` gives the `Replica` (`:835`).
6. The journal releases its IOPS slot and calls the replica's callback.

**Q2.** Running callbacks inline would make them re-enter IO. A callback typically issues a new IO, which can trigger `flush_submissions`, which can call `flush_completions` again, which runs more callbacks. The stack then grows with the number of chained completions.

Scenario: the journal's recovery reads 1,024 slots. Each callback starts the next read; with inline callbacks and synchronously completing reads, that's 1,024 nested frames and a stack overflow, or at least unreadable stack traces. With the `completed` queue, every callback starts from a shallow, constant-depth stack, and the order is FIFO and deterministic. `next_tick` exists for the same reason.

**Q3.**
- Storage reads: `iops_read_max` = 8 + 1 + 32 + 1 = **42**. Writes: `iops_write_max` = 32 + 2 + 32 + 1 = **67**. Total **109**.
- The ring isn't a correctness problem. `enqueue` flushes and retries when the SQ is full, and in-flight ops live in the kernel, not in the SQ. It only costs an extra syscall in a burst (logged as a warning to "consider resizing").
- Network ops share the ring too: an `accept`, a `recv` per connection and `send`s. That's why 128 SQEs is comfortable but not huge.

**Q4.** In real TigerBeetle, the first thing to notice is not IO at all. Growing the list after init calls `alloc`/`remap` on the replica's `StaticAllocator` in state `.static`, so `assert(self.state == .init)` fires (`src/static_allocator.zig:60`). The journal's recovery reads start only *after* `transition_from_init_to_static` (`replica.zig:739` vs `:759`). Concepts 11 and 12 guard each other.

Without that guard (say, in an ashi port without the allocator lock), growth moves every `Read` to a new address. Each in-flight read's SQE still carries the *old* address of its embedded `IO.Completion` in `user_data`. When the CQE arrives, `@ptrFromInt` yields a pointer into freed memory, and `complete()` calls through a garbage `callback` or writes `result` into memory someone else now owns. It's a use-after-free performed on your behalf by the kernel, and it surfaces in the IO layer (`run_callback`/`complete`), far from the refactor that caused it.

This is why pointer stability (static allocation, in-place init) is load-bearing, not stylistic.

**Q5.** A good answer for (a):
- **Bounds:** every component has a fixed `[Slot; N]` + bitset, and the global limit is the sum, asserted in the simulator, like TigerBeetle's priority-queue capacity check.
- **Cancellation:** an explicit stage enum with a `Canceling` state; submit `IORING_OP_ASYNC_CANCEL` or simply wait for the CQE; never free a slot until its CQE arrives.
- **Simulation:** IO is a trait (`submit(op, user_data)`, `poll_completions()`), with a seeded in-memory implementation that reorders and fails ops deterministically.

A good answer for (b):
- **Bounds:** a fixed task table allocated at init; spawn returns `Err` when full.
- **Cancellation:** futures own their buffers, and drop must not free a buffer the kernel still holds. Park the buffer in a "zombie" slot until its CQE arrives.
- **Simulation:** the executor's run queue and the IO driver are both yours, so seed them.

Either way: no work-stealing, no per-task heap allocation after init.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
