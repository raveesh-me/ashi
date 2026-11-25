# Concept 8: Control logic kept out of the data loop

**Module:** M1 Single-node speed · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Control Plane / Data Plane Separation" (lines 408–422) and "Mechanical Sympathy" (358–362)
- `../tigerbeetle/docs/TIGER_STYLE.md`, sections "Safety" (function shape, "push `if`s up and `for`s down") and "Performance"
- `../tigerbeetle/src/constants.zig` (the `verify` doc comment, lines 776–795)
- `../tigerbeetle/src/tigerbeetle.zig` (`Operation`, `EventType`, build-mode check)
- `../tigerbeetle/src/state_machine.zig` (`commit`, `execute_multi_batch`, `execute_create`, `create_transfer`)
- `../tigerbeetle/src/lsm/compaction.zig` (`values_merge`, `values_copy`)
- `../tigerbeetle/src/vsr/replica.zig` (`PipelineQueue.push_request`, `push_prepare`, `verify`)
- `../tigerbeetle/build.zig` (`config_verify` default)
- Line numbers are from the clone at commit `a2a3c70`.

## 1. The problem

A database does two very different kinds of work:

- **Deciding:** which message to process next, whether it is valid, which operation it is, where its results go, whether the view changed. These are decisions with many branches, but they happen once per message.
- **Doing:** applying debit/credit rules to each of 8,189 transfers, or merging 4,094 values per block during compaction. This is the same small computation repeated thousands of times.

TigerBeetle calls these the **control plane** and the **data plane** (`ARCHITECTURE.md:408–418`). The pilot is the control plane; the jet engines are the data plane.

> "In general, control plane is O(1) to data plane's O(N)." (`ARCHITECTURE.md:417`)

When the two are mixed, both suffer:

- **The data loop slows down.** Every decision made per item costs a branch, a possible misprediction, and blocked compiler optimizations such as vectorization and bounds-check hoisting.
- **Safety gets traded away.** If expensive checks run per item, teams disable them in production. Then production runs code paths that were never checked.

## 2. The obvious solution, and why it fails

The obvious design is a heterogeneous stream: each event says what it is, and the loop dispatches per event.

```rust
for event in events {
    match event {                         // decided per item
        Event::CreateAccount(a)  => create_account(a),
        Event::CreateTransfer(t) => create_transfer(t),
        Event::Lookup(id)        => lookup(id),
    }
    verify_all_invariants(&state);        // O(state) per item, so it gets disabled in prod
}
```

Or, more "object-oriented", `Vec<Box<dyn Event>>` with a virtual `apply()` per item.

What goes wrong:

- **Unpredictable branches.** With mixed event types, the branch predictor guesses the `match` wrong often. A mispredict costs on the order of 15–20 cycles on modern cores (a general hardware figure, not from the clone). Against a ~3,000-cycle budget per transfer, a few of those add up.
- **No specialization.** An indirect call through `dyn` cannot be inlined, so the compiler cannot specialize the loop body for one event type.
- **Scattered memory.** Mixed event sizes break fixed-stride layout, so you cannot cast the body to `&[Transfer]` (Concept 9).
- **Assertions become a performance problem.** A full invariant check per event is O(N²) per batch. It gets put behind `debug_assert!` and disappears from production.

## 3. What TigerBeetle does

### 3.1 Homogeneous batches: one operation per message

A message's header carries **one** `operation`. Every event in the body has the same type. The `Operation` enum is at `src/tigerbeetle.zig:679`. The mapping from operation to event type is a comptime function (`tigerbeetle.zig:711`):

```zig
pub fn EventType(comptime operation: Operation) type {
    return switch (operation) {
        .create_accounts => Account,
        .create_transfers => Transfer,
        .lookup_accounts => u128,
        ...
    };
}
```

The docs state the purpose (`ARCHITECTURE.md:360–362`):

> "Events come in batches and each batch is homogeneous --- either all transfers, or all accounts. The event-kind branching is moved out of the inner loop."

### 3.2 Dispatch once, then loop with the decision already made

`StateMachine.commit` switches on the runtime operation **once per prepare** (`src/state_machine.zig:2561–2574`):

```zig
const result: usize = switch (operation) {
    .pulse => self.execute_pulse(timestamp),
    inline .create_accounts,
    .create_transfers,
    .lookup_accounts,
    .lookup_transfers,
    ...
    => |operation_comptime| self.execute_multi_batch(
        timestamp,
        operation_comptime,
        message_body_used,
        output_buffer,
    ),
    ...
};
```

The `inline` prong turns the runtime value into a **comptime** value, `operation_comptime`. Zig then compiles a separate copy of `execute_multi_batch`, and of `execute_create` below it, **for each operation**. This is monomorphization, just like Rust generics.

Inside `execute_create` (`state_machine.zig:2976–3187`) the per-event loop still contains `switch (operation)` statements. But `operation` is a `comptime` parameter, so each switch is resolved at compile time and leaves no branch in the machine code (`state_machine.zig:3060–3070`):

```zig
const create_result = switch (operation) {
    .create_accounts, ... => self.create_account(timestamp_event, event),
    .create_transfers, ... => self.create_transfer(timestamp_event, event),
    else => comptime unreachable,
};
```

The flow from one runtime decision to a specialized loop:

```
prepare (header.operation = create_transfers, body = N × 128 B)
   │
   ▼  control plane: O(1) per prepare
commit()  ── one runtime switch, `inline` → comptime operation
   │
   ▼
execute_multi_batch(comptime .create_transfers)   ── per sub-batch (few)
   │
   ▼  data plane: O(N), specialized
execute_create(comptime .create_transfers)
   for (events) |*event| { create_transfer(...) }  ← no per-event type dispatch
```

### 3.3 Batch-level facts are computed once

Several decisions that *could* be made per event are made once per batch:

- **Import mode is decided by the first event** (`state_machine.zig:3000–3002`):

  ```zig
  // The first event determines the batch behavior for
  // importing events with past timestamp.
  const batch_imported = events.len > 0 and events[0].flags.imported;
  ```

  Later events only compare against `batch_imported`. A mixed batch is rejected per event with `imported_event_expected` / `imported_event_not_expected`. It is never handled as a mixed mode.

- **Timestamps are arithmetic, not a clock call per event** (`state_machine.zig:3005`):

  ```zig
  const timestamp_event = timestamp - events.len + index + 1;
  ```

  The primary reserves a block of timestamps for the whole batch once, in `prepare()` (`state_machine.zig:1146–1177`). Concept 19 covers timestamps as keys.

- **The pulse.** Pending-transfer expiry runs as its own operation (`.pulse`) with its own prepare. It is never checked inside the transfer loop.

### 3.4 "Push ifs up and fors down"

TigerStyle turns this into a coding rule (`TIGER_STYLE.md`, Safety section):

> "Centralize control flow. When splitting a large function, try to keep all switch/if statements in the "parent" function, and move non-branchy logic fragments to helper functions. ... In other words, 'push `if`s up and `for`s down'."

The clearest example is compaction's merge loop. Its hot functions take plain slices and no `self` (`src/lsm/compaction.zig:2042–2047`):

> "The three functions below are hot CPU loops doing the actual merging, TigerBeetle's data plane. To reduce the probability of the optimizer getting confused over pointers, don't use 'self' and instead specify all inputs and outputs explicitly. Its the caller's job to apply control plane changes to the compaction state."

`values_merge` (`compaction.zig:2228`) takes `values_target`, `values_source_a`, `values_source_b` and a `drop_tombstones` flag. It returns a small `MergeResult`. The caller (control plane) updates the compaction's positions.

TigerStyle's Performance section says the same: "extract hot loops into stand-alone functions with primitive arguments without `self`".

### 3.5 Aggressive assertions where they are cheap

Batching makes the control plane cheap. One prepare costs O(1) control work for ~8 K transfers, so the control plane can afford very expensive checks. The docs: "it can use very aggressive assertions (up to spending O(N) time to verify O(1) operation)" (`ARCHITECTURE.md:421–422`).

The rule set is written down in `constants.zig:776–795`:

> "In the control plane (eg, vsr proper) assert unconditionally. Due to batching, control plane overhead is negligible. It is acceptable to spend O(N) time to verify O(1) computation.
>
> In the data plane (eg, lsm tree), finer grained judgement is required. Do an unconditional O(1) assert before an O(N) loop (e.g, a bounds check). Inside the loop, it might or might not be feasible to add an extra assert per iteration. In the latter case, guard the assert with `if (constants.verify)` ...
>
> In the data plane, never use O(N) asserts for O(1) computations"

Examples of each tier in real code:

| Tier | Example | Where |
|---|---|---|
| Control plane, O(N) check for an O(1) push, **always on** | `assert_request_queue` loops over every queued request to assert the new request's client is not already queued | `src/vsr/replica.zig:12346–12351`, called from `push_request` at `12337` |
| Control plane, O(1) hash-chain checks, always on | `push_prepare` asserts `op == tail.op + 1` and `parent == tail.checksum` | `replica.zig:12353–12366` |
| Control plane, O(N) full verify, under `constants.verify` | `PipelineQueue.verify` walks the whole pipeline and re-checks the hash chain | `replica.zig:12179–12215` |
| Data plane, O(1) before the loop | `values_merge` asserts every slice length against `Table.data.value_count_max` before merging | `compaction.zig:2234–2239` |
| Data plane, O(1) after the loop | `execute_multi_batch` asserts `execute_timestamp == timestamp` and the batch counts match | `state_machine.zig:2730–2731` |
| Data plane, per item, under `constants.verify` | `create_transfer` checks that every key it reads was enqueued for prefetch | `state_machine.zig:3713–3716` |

When is `constants.verify` on? `build.zig:200–202`:

```zig
.config_verify = b.option(bool, "config_verify", "Enable extra assertions.") orelse
    // If `config_verify` isn't set, disable it for `release` builds; otherwise, enable it.
    (options.mode == .Debug),
```

Plain `assert` stays on in release builds, because TigerBeetle refuses to compile in unchecked modes (`src/tigerbeetle.zig:943–946`):

```zig
switch (builtin.mode) {
    .Debug, .ReleaseSafe => {},
    .ReleaseFast, .ReleaseSmall => @compileError("safety checks are required for correctness"),
}
```

So production runs with every unconditional assertion. Only the per-item and O(N) "verify" checks are compiled out. The control/data split is what makes that affordable.

### 3.6 Why the split is safe as well as fast

Assertions turn correctness bugs into crashes (TigerStyle: "Assertions downgrade catastrophic correctness bugs into liveness bugs"). The simulator (Concept 29) runs millions of these checks. The `constants.verify` comment also notes the testing trade-off: "Slow thorough assertions decrease the overall test coverage", because fuzzers run fewer iterations. That is why per-item O(N) checks are banned even in test builds of the data plane.

## 4. What it means for ashi

### Rust features that help

- **Generics and monomorphization** are Rust's `comptime` for this purpose. One `match` on the operation per message, then call a generic, specialized loop:

```rust
trait Op { type Event: Pod; type Result: Pod; fn apply(sm: &mut Sm, ts: u64, e: &Self::Event) -> Self::Result; }

fn commit(sm: &mut Sm, op: Operation, body: &[u8], out: &mut [u8]) -> usize {
    match op {                                   // control plane: once per prepare
        Operation::CreateTransfers => execute::<CreateTransfers>(sm, body, out),
        Operation::CreateAccounts  => execute::<CreateAccounts>(sm, body, out),
        /* ... */
    }
}

fn execute<O: Op>(sm: &mut Sm, body: &[u8], out: &mut [u8]) -> usize {
    let events: &[O::Event] = cast_slice(body);  // data plane: specialized per O
    /* for e in events { O::apply(...) } */
}
```

- **`const` generics or associated `const`s** carry batch-wide flags (for example `const IMPORTED: bool`) into the loop with no runtime branch. Use them only where the branch really matters.
- **Assertion tiers map to Rust:**
  - unconditional O(1) → `assert!`;
  - per-item or O(N) checks → `if cfg!(feature = "verify") { assert!(...) }`, or a `const VERIFY: bool` derived from a build feature.
  - Avoid `debug_assert!` for anything TigerBeetle would keep on in production.

### Patterns

- Hot loops as free functions over slices (`fn merge(a: &[V], b: &[V], out: &mut [V]) -> MergeResult`), not `&mut self` methods. This mirrors `values_merge`, and it also helps the borrow checker and the optimizer's alias analysis.
- Assert slice lengths **before** the loop (`assert!(out.len() >= a.len() + b.len())`). That lets LLVM hoist bounds checks out of the loop.
- Keep error paths out of line: `#[cold]`, `#[inline(never)]` on functions that build error results. Check your toolchain for stable branch hints (for example `std::hint::cold_path`) before relying on them; they have been changing.

### Pitfalls specific to Rust

- **Release profile defaults differ from TigerBeetle's.** Rust disables integer overflow checks in `--release` by default. TigerBeetle runs ReleaseSafe. Set `overflow-checks = true` and `panic = "abort"` in `[profile.release]`, and measure the cost instead of assuming it.
- **`Box<dyn Trait>` per event** or `enum` events with mixed sizes silently reintroduce per-item dispatch.
- **Iterator chains** with closures that capture `&mut self` can block optimizations that a plain indexed loop over slices gets.

**ashi stance:** _to be decided once the drill is done_

Suggested: **adopt**. Generics give Rust the same comptime specialization, and the assertion tiers are a policy, not a language feature, so they port directly.

## 5. Drill questions

### Q1. Recall

State TigerBeetle's three assertion rules from `constants.verify` (control plane, data plane before/after loops, data plane inside loops). Give one concrete code example of each.

> _Your answer:_

### Q2. Mechanism

In `execute_create`, the per-event loop contains `switch (operation)`. Explain why this costs nothing at runtime. What in `commit()` makes it so?

> _Your answer:_

### Q3. Back-of-envelope

A control-plane check costs 50 µs (an O(N) walk over the pipeline and client table) and runs once per prepare. A full prepare holds 8,189 transfers and the target is 1 M transfers/s.

- What fraction of CPU time does the check consume?
- What if the same check ran once per transfer instead?

> _Your answer:_

### Q4. What breaks if…

…the API allowed mixed batches (accounts and transfers in one message, each event tagged)? List the effects on the commit loop, on zero-copy decoding, on the result encoding, and on assertion strategy.

> _Your answer:_

### Q5. ashi design (open-ended)

Rust's release profile turns off overflow checks, and teams habitually use `debug_assert!`. Design ashi's assertion policy: what is always on, what is behind a `verify` feature, how you keep CI and the simulator running with `verify` on, and how you would measure that production-on assertions cost under ~5%.

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- **Control plane:** assert unconditionally, even O(N) checks for O(1) operations. Example: `assert_request_queue` loops over every queued request on each `push_request` (`replica.zig:12337–12351`). Another: `push_prepare` asserts the hash chain on every push.
- **Data plane, around loops:** one unconditional O(1) assert before the O(N) loop, and O(1) postconditions after. Examples: `values_merge` length asserts (`compaction.zig:2234–2239`); `execute_multi_batch` asserting `execute_timestamp == timestamp` (`state_machine.zig:2730`).
- **Data plane, inside loops:** per-iteration asserts only if cheap; otherwise guard with `if (constants.verify)`. Never put an O(N) assert inside an O(1) data-plane step. Example: `create_transfer`'s prefetch-key check under `constants.verify` (`state_machine.zig:3713–3716`).

**Q2.** `commit()` uses an `inline` switch prong, which binds `operation_comptime` as a compile-time value. `execute_multi_batch` and `execute_create` take `comptime operation: Operation`, so Zig generates one copy per operation. In each copy, `switch (operation)` has a known value and folds to the single taken branch. The `else => comptime unreachable` arms prove at compile time that the other branches are impossible.

**Q3.**
- Prepares per second at 1 M transfers/s: `1,000,000 / 8,189 ≈ 122`. CPU: `122 × 50 µs ≈ 6.1 ms` per second, about **0.6%**.
- Per transfer: `1,000,000 × 50 µs = 50 s` per second, **50×** over budget. Impossible.

This is the whole argument for doing expensive checks per batch: batching makes the control plane nearly free.

**Q4.**
- **Commit loop:** needs a per-event type branch (mispredicts when mixed) and cannot be specialized per operation; inlining suffers.
- **Zero-copy decoding:** events no longer sit at a fixed 128-byte stride if sizes differ (a lookup is a 16-byte `u128`). You need a parsing pass or tagged fixed-size slots that waste space.
- **Result encoding:** result types differ (`CreateAccountResult` vs `CreateTransferResult` vs `Account`), so the reply has mixed sizes too.
- **Prefetch:** must handle all event kinds in one pass, and the prefetch-size bounds must cover the worst mix.
- **Assertions:** the batch-wide invariants (one event size, `@divExact` lengths, `batch_imported` from the first event) no longer hold. Per-item checks multiply.
- **Static sizing:** bounds (`batch_max`, stash sizes) must cover the worst mix, which costs more memory.

**Q5.** Open-ended. A strong answer:
- `assert!` (always on) for O(1) invariants and for every control-plane check, including O(N) ones per message.
- Overflow checks on in release; `panic = "abort"`.
- A `verify` cargo feature for per-item and O(N) data-plane checks. It is on in tests, CI, fuzzers and the simulator, and off in production builds. Mirror TigerBeetle's `config_verify` default: on in debug, off in release.
- Ban `debug_assert!` by lint, or alias it to the `verify` tier, so no invariant is silently dropped.
- **Measure:** run the benchmark with assertions compiled out (a special `unchecked` profile used *only* for measurement) versus the production profile, at saturation and at p99. Keep it well under 5%: the `constants.verify` comment says a 5% production slowdown "might be deemed critical", while tests tolerate up to 5x.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
