# Concept 30: TigerStyle coding rules

**Module:** M5 Correctness · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/TIGER_STYLE.md`, all sections ("Technical Debt", "Safety", "Performance", "Developer Experience", "Off-By-One Errors", "Style By The Numbers", "Dependencies")
- `../tigerbeetle/src/constants.zig`: comptime assertion blocks (lines 34–71, 115–121) and the `verify` flag (lines 776–795)
- `../tigerbeetle/src/tigerbeetle.zig:111-115` (`Transfer` size and alignment asserts)
- `../tigerbeetle/src/vsr/journal.zig`: lines 47–52, 64, 368–369, 1789–1803
- `../tigerbeetle/src/vsr/grid.zig`: `write_block` (940–983) and `read_block_validate` (1421–1446)
- `../tigerbeetle/src/vsr/replica.zig:3897-3903` (bounded loop)
- `../tigerbeetle/src/stdx/stdx.zig`: `div_ceil` (81), `maybe` (401)
- `../tigerbeetle/build.zig:89` (default build mode)
- `src/tidy.zig` as of the parent of commit `2f45ef6d4` (read with `git show`; the file is no longer in the tree)
- `../tigerbeetle/CHANGELOG.md`, entries for PR #2148, PR #3543 and commit `b693fff`

## 1. The problem

A financial ledger has no "minor" bugs.

- A wrong balance is a correctness bug. It is silent, and it spreads to every downstream system.
- In a replicated system (Concept 20), a bug that runs differently on two replicas makes them disagree. Now you have two truths.
- TigerBeetle's design goals are "safety, performance, and developer experience. In that order."

Most catastrophic failures are not exotic. TigerStyle quotes a study of production failures in distributed data systems (Yuan et al., OSDI 2014):

> "almost all (92%) of the catastrophic system failures are the result of incorrect handling of non-fatal errors explicitly signaled in software."

So the problem is: how do you write code where bugs are found early, fail loudly, and cannot quietly corrupt data? Testing alone (Concepts 29 and 31) is not enough. Fuzzers find bugs only where the code *notices* something went wrong.

## 2. The obvious solution, and why it fails

The usual answer is "write clean code, add tests, review carefully." That fails in three ways.

**1. Bugs that do not crash are invisible to fuzzers.** A fuzzer or simulator only reports a failure when something checks. Picture a function that computes `debits_posted` with an off-by-one. Without an assertion, the VOPR (Concept 29) runs millions of seeds and sees nothing. The wrong number goes to disk. With one assertion such as `assert(debits_posted <= debits_max)`, the first bad seed crashes, and you get a reproducible seed.

**2. Assertions compiled out in production.** Many codebases use C's `assert` or Rust's `debug_assert!`. These vanish in release builds. The checks protect the test environment, not the customer's data.

**3. Style rules without numbers drift.** "Keep functions short" means nothing. Every function grows by one line at a time. Nobody notices until it is 400 lines long.

**Back-of-envelope: why "asserts are too slow" is usually wrong.** Suppose one check costs 1 µs, and the system runs 1,000,000 transfers per second.

```
check per transfer: 1,000,000 × 1 µs = 1 s of CPU per second  → 100% of a core
check per batch:    1,000,000 / 8,190 ≈ 122 batches/s
                    122 × 1 µs ≈ 122 µs of CPU per second       → ~0.01% of a core
```

(8,190 transfers per batch comes from a 1 MiB message minus a 256-byte header, divided by 128 bytes per transfer; see Concept 7.) The same check is free or ruinous depending on *where* you put it. TigerStyle turns that into a rule.

## 3. What TigerBeetle does

TigerStyle is a written style guide, `docs/TIGER_STYLE.md`. It extends NASA's "Power of Ten" rules for safety-critical code. Below are the rules that matter most, each with where it shows up in the source.

### 3.1 Assertions: the core rule

> "Assertions detect programmer errors. Unlike operating errors, which are expected and which must be handled, assertion failures are unexpected. The only correct way to handle corrupt code is to crash. Assertions downgrade catastrophic correctness bugs into liveness bugs."

Two kinds of error, handled in opposite ways:

```
operating error  (disk returns bad bytes, client sends bad input, network drops)
    → expected → handle it, return an error code, repair
programmer error (an invariant the code itself should guarantee is false)
    → unexpected → crash immediately
```

"Downgrade correctness bugs into liveness bugs" means: a crashed replica is unavailable (a liveness problem), but it did not write a wrong balance (a safety problem). With 3 or more replicas (Concept 21), the cluster keeps running.

**Assertions stay on in production.** `build.zig:89` defaults to `.ReleaseSafe`. In Zig, that mode keeps `assert`, bounds checks and integer-overflow checks.

**Density.** TigerStyle asks to assert "all function arguments and return values, pre/postconditions and invariants", with "a minimum of two assertions per function" on average.

A real example, the preconditions at the top of `Journal.write_prepare` (`src/vsr/journal.zig:1796-1803`):

```zig
assert(journal.status == .recovered);
assert(message.header.command == .prepare);
assert(message.header.operation != .reserved);
assert(message.header.size >= @sizeOf(Header));
assert(message.header.size <= message.buffer.len);
assert(journal.has_header(message.header));
assert(journal.writing(message.header) == .none);
if (replica.solo()) assert(journal.writes.executing() == 0);
```

Notice three sub-rules in action:
- **Split compound assertions.** `assert(a); assert(b);`, never `assert(a and b);`. When it fails, you know which half failed.
- **Implications as a one-line `if`.** `if (a) assert(b)`, as on the last line.
- **Bounds on both sides.** `size >= @sizeOf(Header)` and `size <= buffer.len`.

A rough grep of the clone counts about 1,584 `assert(` calls in `src/vsr/replica.zig` across about 268 function declarations: roughly 6 per function.

### 3.2 Pair assertions

> "For every property you want to enforce, try to find at least two different code paths where an assertion can be added. For example, assert validity of data right before writing it to disk, and also immediately after reading from disk."

The grid (Concept 17) shows the pair. On the write path, `write_block` zeroes the padding after a block's real content (`src/vsr/grid.zig:966-967`):

```zig
// Zero sector padding.
@memset(block.*[header.size..vsr.sector_ceil(header.size)], 0);
```

On the read path, `read_block_validate` checks the same property (`src/vsr/grid.zig:1440-1442`):

```zig
if (!stdx.zeroed(block[header.size..vsr.sector_ceil(header.size)])) {
    return .invalid_padding;
}
```

Note the read side returns an error, not an assertion. Bytes from disk can be corrupted by the hardware (Concept 14). That is an *operating* error, so it is handled. The same function does `assert(header.size <= constants.block_size)`, but only *after* the checksum passed. Once the checksum is valid, a bad size would be a programmer error.

Zeroed padding matters for two reasons. TigerStyle calls unzeroed padding a "buffer bleed": it can leak memory contents, and it breaks the byte-identical storage that Concept 20 requires.

### 3.3 Positive space and negative space

> "The golden rule of assertions is to assert the _positive space_ that you do expect AND to assert the _negative space_ that you do not expect."

The boundary between valid and invalid is where bugs live. Example: when a journal is first created, *no* header should look valid yet (`src/vsr/journal.zig:368-369`):

```zig
for (journal.headers) |*h| assert(!h.valid_checksum());
for (journal.headers_redundant) |*h| assert(!h.valid_checksum());
```

TigerBeetle adds a third tool, `maybe` (`src/stdx/stdx.zig:401`):

```zig
/// `maybe` is the dual of `assert`: it signals that condition is sometimes true
///  and sometimes false.
pub fn maybe(ok: bool) void {
    assert(ok or !ok);
}
```

It checks nothing. It *documents* that both outcomes are expected, for example `maybe(self.status == .normal)`. It is used 57 times in `replica.zig`. A reader learns that the author thought about both cases.

### 3.4 Compile-time assertions

> "Compile-time assertions are extremely powerful because they are able to check a program's design integrity _before_ the program even executes."

In Zig, a `comptime { ... }` block runs during compilation. If an `assert` inside fails, the build fails.

**Type sizes** (`src/tigerbeetle.zig:111-115`):

```zig
comptime {
    assert(stdx.no_padding(Transfer));
    assert(@sizeOf(Transfer) == 128);
    assert(@alignOf(Transfer) == 16);
}
```

**Relationships between constants** (`src/constants.zig:44-71`). The checkpoint interval must leave room in the write-ahead log, or a log entry could be overwritten before a quorum has checkpointed it:

```zig
assert(vsr_checkpoint_ops + lsm_compaction_ops + pipeline_prepare_queue_max * 2 <=
    journal_slot_count);
assert(vsr_checkpoint_ops >= pipeline_prepare_queue_max);
assert(vsr_checkpoint_ops >= lsm_compaction_ops);
assert(vsr_checkpoint_ops % lsm_compaction_ops == 0);
```

`constants.zig` contains about 25 `comptime` blocks. If someone changes a config value to an unsafe combination, the program does not build. That is a durability bug caught with zero runtime cost.

### 3.5 Where to assert: control plane vs data plane

`constants.verify` (`src/constants.zig:776-795`) turns the back-of-envelope from section 2 into policy:

> "In the control plane (eg, vsr proper) assert unconditionally. Due to batching, control plane overhead is negligible. It is acceptable to spend O(N) time to verify O(1) computation.
>
> In the data plane (eg, lsm tree), finer grained judgement is required. Do an unconditional O(1) assert before an O(N) loop (e.g, a bounds check). Inside the loop, it might or might not be feasible to add an extra assert per iteration. In the latter case, guard the assert with `if (constants.verify)`, but prefer an unconditional assert unless benchmarks prove it to be costly."

The comment also gives the budget: "For production, 5% slow down might be deemed critical, tests tolerate slowdowns up to 5x." It warns that slow checks reduce test coverage, because a slower simulator runs fewer seeds. This is TigerBeetle's equivalent of a `debug_assert`, but explicit and rare (about 78 uses in `src/`). See Concept 8 for the control/data plane split.

### 3.6 Limits on everything: bounded loops, no recursion

> "Do not use recursion to ensure that all executions that should be bounded are bounded."
> "all loops and all queues must have a fixed upper bound"

Recursion depth depends on input. Stack size is fixed. So recursion is an unbounded resource. A loop with a written bound turns "this terminates" from a hope into a checked fact.

The idiom: loop up to the bound plus one, and declare that hitting the end is impossible (`src/vsr/replica.zig:3897-3903`):

```zig
for (0..constants.grid_scrubber_reads_max + 1) |_| {
    const scrub_next = self.grid_scrubber.read_next();
    if (!scrub_next) {
        if (self.grid_scrubber.tour == .done) self.grid_scrubber.wrap();
        break;
    }
} else unreachable;
```

In Zig, a `for ... else` runs the `else` branch only if the loop finished without `break`. `unreachable` crashes in ReleaseSafe. So if `read_next` ever misbehaves, the replica crashes instead of spinning forever.

The one loop that should never end, the event loop, is `while (true)` in `command_start` (`src/tigerbeetle/main.zig:591`). The rule asks for this to be asserted. Here it is simply a `while (true)` with no `break`, so the compiler knows the code after it is unreachable.

Static allocation (Concept 11) is the memory version of the same rule. Every queue and pool has a size fixed at startup.

### 3.7 The 70-line function limit

> "There's a sharp discontinuity between a function fitting on a screen, and having to scroll to see how long it is. For this physical reason we enforce a **hard limit of 70 lines per function**."

How to split well:
- Good function shape is "the inverse of an hourglass": few parameters, a simple return type, lots of logic inside.
- **"Push `if`s up and `for`s down."** Keep branching in the parent function. Move straight-line logic into helpers.
- Keep state changes in the parent too. Helpers compute what should change; the parent applies it. Leaf functions stay pure.

> ⚠️ **Docs vs code: the 70-line limit.** The doc says "hard limit" and "enforce". The code does not enforce it as stated.
> - The checker was `src/tidy.zig`. Its function-length check used a "red zone" (`tidy.zig`, `function_length_red_zone = .{ .min = 70, .max = 73 }`, both exclusive). It flagged only functions of **71 or 72 lines**. The comment says: "We ratchet 70-lines-per-function TigerStyle rule from the bottom up... The most values is in preventing originally small functions to grow over time." The CHANGELOG entry for PR #3543 says the same.
> - My rough scan (single-line signatures only, generic type-returning functions excluded) finds about 155 of about 3,550 functions over 70 lines. Examples: `on_prepare` at `src/vsr/replica.zig:2014` runs about 226 lines; `command_start` at `src/tigerbeetle/main.zig:308` is also far over.
> - `src/tidy.zig` is **no longer in this clone**. Commit `2f45ef6d4` ("tidy: move. We now have a separate place for such tooling.") deleted it and removed the "Run lint checks" step from `docs/internals/HACKING.md`. I could not see where it moved or whether its rules changed.
>
> The code wins: in practice the 70-line limit is a strong target with a ratchet that stops functions from creeping just past it, not a hard limit.

### 3.8 Explicitly sized integers

> "Use explicitly-sized types like `u32` for everything, avoid architecture-specific `usize`."

The reason: on-disk and on-wire formats must be identical on every machine (Concept 16). A `usize` is 8 bytes on one target and 4 on another. Every field of `Transfer` is a fixed-width integer (`u128`, `u64`, `u32`, `u16`) or a flags struct packed into a `u16`, which is how the comptime size assert in 3.4 can say exactly 128.

> ⚠️ **Docs vs code: `usize`.** The source has about 1,075 uses of `usize` outside vendored code. Zig indexes slices with `usize`, so in-memory indices use it, for example `const Slot = struct { index: usize };` (`src/vsr/journal.zig:64`). In practice the rule holds for **persisted and wire data and for configuration**, not for every local index.

### 3.9 Naming

- **Units and qualifiers last**, sorted by descending significance: `latency_ms_max`, not `max_latency_ms`. Related names then line up and sort together (`latency_ms_min`, `latency_ms_max`). The source follows this: `message_size_max`, `clients_max`, `vsr_releases_max`, `grid_scrubber_reads_max` in `constants.zig`.
- **Nouns over adjectives**: `replica.pipeline`, not `replica.preparing`. A noun works as a section heading and composes, as in `pipeline_max`.
- **Same-length related names**: `source`/`target`, not `src`/`dest`, so `source_offset` and `target_offset` line up.
- **Helper names carry the caller**: `read_sector()` and `read_sector_callback()`. Callbacks go last in parameter lists, because they run last.
- **No abbreviations**, except loop-local integers in sorts or matrix math.
- **Acronyms in capitals**: `VSRState`, not `VsrState`.
- **Don't overload terms.** "Two-phase commit transfers" was renamed to "pending transfers" because "two-phase commit" already meant something in consensus.
- **Options structs** when two arguments of the same type could be swapped: "A function taking two `u64` must use an options struct."

### 3.10 Off-by-one discipline: index, count, size

> "The usual suspects for off-by-one errors are casual interactions between an `index`, a `count` or a `size`. These are all primitive integer types, but should be seen as distinct types, with clear rules to cast between them."

```
index  (0-based)  ──+1──▶  count  (1-based)  ──×unit──▶  size  (bytes)
  slot.index = 1023        journal_slot_count = 1024     1024 × 256 B = 256 KiB
```

Division is also explicit. `@divExact` crashes if there is a remainder; `@divFloor` and `stdx.div_ceil` say which way you round. Real use (`src/vsr/journal.zig:47-48`):

```zig
const headers_per_sector = @divExact(constants.sector_size, @sizeOf(Header));
const headers_per_message = @divExact(constants.message_size_max, @sizeOf(Header));
```

`stdx.div_ceil` (`src/stdx/stdx.zig:81`) asserts at compile time that both operands are unsigned and the denominator is positive.

Related: state invariants positively. `if (index < count)` reads naturally; `if (index >= count)` makes the reader negate.

### 3.11 Handle all errors

Every error is either handled or explicitly propagated. The tidy ban list (below) also banned `== error.` and `!= error.`, with the replacement "switch to avoid silent anyerror upcast". A `switch` on an error set must list every case, so a new error cannot slip through unhandled.

Also: **pass options explicitly** instead of relying on library defaults, because a changed default is a silent behavior change. Example from the doc: `@prefetch(a, .{ .cache = .data, .rw = .read, .locality = 3 })`.

### 3.12 Zero technical debt

> "TigerBeetle has a 'zero technical debt' policy. We do it right the first time... We may lack crucial features, but what we have meets our design goals."

The idea: a design problem costs least while the code is "hot". Known showstoppers, such as "potential memcpy latency spikes, or exponential complexity algorithms", are fixed before merge. The trade is scope, not quality: ship fewer features, each one solid.

Note: zero debt does not mean zero `TODO` comments. The source has about 213 `TODO`s, mostly future work. What tidy banned was `FIXME` (a "leftover, remove before merge" reminder) and `dbg(`.

### 3.13 The performance sketch

> "The best time to solve performance, to get the huge 1000x wins, is in the design phase, which is precisely when we can't measure or profile."

The method:
- Sketch the **four resources** (network, disk, memory, CPU) on **two axes** (bandwidth, latency).
- **Optimize the slowest resource first**, after weighting by how often it is used. "A memory cache miss may be as expensive as a disk fsync, if it happens many times more."
- Batch to amortize every resource (Concept 7).
- Extract hot loops into standalone functions "with primitive arguments without `self`", so the compiler need not prove struct fields stay in registers.

Section 2's assertion-cost sketch is one of these.

### 3.14 Style by the numbers

| Rule | Number | Why |
|---|---|---|
| Function length | 70 lines | Fits on one screen |
| Line length | 100 columns, "without exception" | Two files side by side |
| Indentation | 4 spaces | Visible at a distance |
| Assertions | ≥ 2 per function, on average | Fuzzers need checks to find bugs |
| Pass by pointer | arguments > 16 bytes go as `*const` | Catch accidental stack copies |
| Braces | always, unless the `if` fits on one line | Defense against "goto fail" bugs |

Plus "all compiler warnings at the compiler's strictest setting", and `zig fmt`.

### 3.15 How the rules were checked: tidy

Until September 2026, `src/tidy.zig` ran as a test. Besides the line and function length checks, it:
- **banned APIs** with a named replacement: `@memcpy(` → `stdx.copy_disjoint`, `std.BoundedArray` → `stdx.BoundedArrayType`, `debug.assert(` → "unqualified assert", `usingnamespace`, `Self = @This()` → "proper type name";
- **banned mixing** bitwise and arithmetic operators without parentheses ("ambiguous precedence");
- flagged **dead declarations**, **untracked or dead files**, and a missing blank line after `defer`;
- per CHANGELOG commit `b693fff`, failed on commits whose metadata showed LLM authorship.

Most of these are mechanical. The important rules (assertion quality, naming, splitting functions well) are enforced by code review, not tools.

## 4. What it means for ashi

### Where Rust helps

- **rustfmt defaults already match.** `max_width` defaults to 100 and indentation to 4 spaces.
- **Braces are mandatory** on every `if`. The "goto fail" class of bug cannot be written.
- **`match` is exhaustive.** That covers "handle all errors" and "switch, don't compare errors", as long as you avoid `_ =>` wildcards.
- **`Result` is `#[must_use]`.** Ignoring an error is a warning by default; make it an error.
- **Newtypes** can make index/count/size *real* distinct types, which Zig's guide only asks you to keep in your head: `struct SlotIndex(u32)`, `struct SlotCount(u32)`, with one `fn count_from_index` that adds 1. Use `std::time::Duration` or a `Ms(u64)` newtype for time.
- **Const assertions** are stable: `const _: () = assert!(...)` runs at compile time.

### Where Rust hurts

- **`debug_assert!` is the default culture.** It disappears in `--release`. TigerStyle wants the opposite.
- **Release builds wrap on integer overflow.** Zig's ReleaseSafe traps. Rust's default release profile sets `overflow-checks = false`, so `u64` overflow silently wraps.
- **Panics unwind by default.** A panic can be caught (`catch_unwind`, or a thread boundary), leaving half-updated state alive. "The only correct way to handle corrupt code is to crash."
- **Indexing wants `usize`.** You cannot avoid it. Store `u32`/`u64`; convert at the index site.
- **No lint for recursion.** As far as I know, neither rustc nor clippy flags general recursion. rustc's `unconditional_recursion` only catches recursion with no base case. Rust-specific trap: the default `Drop` of a `Box`-linked list is recursive and can overflow the stack.
- **`clippy::missing_panics_doc`** (pedantic) fires on public functions that can panic. TigerStyle-dense asserts make it fire nearly everywhere. Allow it, or it buries real warnings.
- **No placement new.** Building a large struct "in place" (TigerStyle's out-pointer `init`) is hard without `unsafe`, which ashi forbids. See Concept 11.

### Patterns

**Assertions on in production, plus a `verify!` for expensive ones:**

```rust
/// Expensive O(N) checks. On in tests, fuzzers and the simulator; off in production.
macro_rules! verify {
    ($($arg:tt)*) => {
        if cfg!(any(test, feature = "verify")) { assert!($($arg)*); }
    };
}
```

Rule for ashi, mirroring `constants.verify`: use `assert!` by default; use `verify!` only for O(N) checks inside data-plane loops; never use `debug_assert!`.

**Compile-time asserts, zero dependencies:**

```rust
const _: () = assert!(core::mem::size_of::<Transfer>() == 128);
const _: () = assert!(core::mem::align_of::<Transfer>() == 16);
const _: () = assert!(VSR_CHECKPOINT_OPS % LSM_COMPACTION_OPS == 0);
const _: () = assert!(usize::BITS == 64); // ashi targets 64-bit only

fn ring<const N: usize>() {
    const { assert!(N > 0 && N.is_power_of_two()) }; // checked per instantiation (Rust 1.79+)
}
```

The `static_assertions` crate (`const_assert!`, `assert_eq_size!`) predates const `assert!`. It is no longer needed, and the zero-dependency rule (Concept 13) argues against it. Zig's `stdx.no_padding` has no std equivalent. A padding check needs either a manual "sum of field sizes == `size_of`" assert, or a derive from `zerocopy`/`bytemuck` (both dependencies).

**Bounded loops:** Rust has no `for ... else`. Use a labeled block that breaks out with a value, and assert on the fall-through:

```rust
let found = 'search: {
    for _ in 0..=READS_MAX {
        if !scrubber.read_next() { break 'search true; }
    }
    false
};
assert!(found, "scrubber exceeded READS_MAX");
```

**Event loop:** give it the type `-> !` so "never returns" is checked by the compiler.

**Split asserts and implication:** `assert!(a); assert!(b);` and `if a { assert!(b); }`. No lint enforces this; it is a review convention.

**Operating errors vs programmer errors:** client input that would overflow a balance is an *operating* error. Use `checked_add` and return a result code, as TigerBeetle returns `overflows_debits`. A broken internal invariant is a programmer error: `assert!`.

### Suggested lint configuration for ashi

Starting from ashi's current lints, add the following. Every name below is a real rustc or clippy lint as far as I know; check each against your toolchain with `cargo clippy -- -W help`.

```toml
# Cargo.toml
[workspace.lints.rust]
unsafe_code = "forbid"
missing_debug_implementations = "warn"
missing_docs = "warn"                     # "always say why"
unreachable_pub = "warn"
unused_qualifications = "warn"
unused_must_use = "deny"                  # handle all errors

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
# Limits (thresholds in clippy.toml)
too_many_lines = "warn"                   # pedantic; 70-line rule
excessive_nesting = "warn"
cognitive_complexity = "warn"             # nursery, opt in
infinite_loop = "warn"                    # restriction: endless loop must be `-> !`
# Explicit integers (Concept 16)
as_conversions = "warn"                   # use From / try_from, not `as`
default_numeric_fallback = "warn"         # no silent i32/f64 literals
# Errors
let_underscore_must_use = "deny"
map_err_ignore = "warn"
wildcard_enum_match_arm = "warn"          # no `_ =>` on enums
# Zero technical debt
todo = "deny"
unimplemented = "deny"
dbg_macro = "deny"
allow_attributes_without_reason = "warn"  # every #[allow] says why
# Noise under TigerStyle-density asserts
missing_panics_doc = "allow"
```

```toml
# clippy.toml
too-many-lines-threshold = 70
excessive-nesting-threshold = 4
disallowed-macros = [
    { path = "std::debug_assert", reason = "use assert! or verify!" },
    { path = "std::debug_assert_eq", reason = "use assert_eq! or verify!" },
]
# Tidy-style ban list: add disallowed-methods / disallowed-types entries
# as ashi grows its own stdx (e.g. ban allocation after init, Concept 11).
```

```toml
# Cargo.toml: make release behave like Zig's ReleaseSafe
[profile.release]
overflow-checks = true
panic = "abort"
```

Notes on the choices:
- **`too_many_lines` counts differently from tidy.** Clippy skips blank and comment-only lines; tidy counted every line from first to last token. Clippy at 70 is somewhat looser.
- **Leave `indexing_slicing` and `arithmetic_side_effects` off globally.** With bounds checks and `overflow-checks = true`, an out-of-range index or overflow *is* a crash, which is exactly an assertion. Turn `arithmetic_side_effects` on only in the state-machine crate, where overflow comes from client input and must be a handled result code.
- **`unwrap_used`:** in TigerStyle, Zig's `.?` is an assertion. `unwrap()` is the same thing. Either allow it, or deny it and require `expect("invariant: ...")` so each one states its invariant.
- **Acronyms:** Rust convention is `VsrState`. Fight the ecosystem here only if you have a reason; TigerStyle itself says "for the rest, follow the [language] style guide."
- **No tool for what matters most.** No lint measures assertion quality, pair assertions, naming with units last, or `if`s up and `for`s down. Put these in a review checklist.

### Pitfalls specific to Rust

- `cargo test` builds in debug with overflow checks on; `cargo build --release` turns them off unless you set the profile. Run the simulator and fuzzers (Concepts 29, 31) on the **release** profile you ship.
- `assert!(x, "{}", expensive())` is fine: the message is only formatted on failure. The *condition* is always evaluated.
- Iterator chains hide loops. They are bounded by the collection, but `std::iter::repeat` and `loop` are not.
- `#[allow(clippy::too_many_lines)]` on a function is the Rust way to opt out. Requiring a reason makes each one a reviewed decision, like tidy's ratchet.

**ashi stance:** _to be decided once the drill is done_

Suggestion: **adopt** the rules (assert-on-in-production, compile-time asserts, bounds, units-last naming, zero debt), and **adapt** the enforcement to clippy plus a release profile that traps overflow and aborts on panic, because Rust's defaults lean the other way.

## 5. Drill questions

### Q1. Recall

Rename these to TigerStyle: `max_latency_ms`, `numRetries`, `dst_off`, `timeout` (a value in milliseconds). Then name two places in `constants.zig` that already follow the rule.

> _Your answer:_

### Q2. Mechanism

In the grid, the write path zeroes block padding, and the read path checks it. But the read path *returns an error* (`.invalid_padding`) instead of asserting. Explain why. Then explain why `read_block_validate` *does* assert `header.size <= constants.block_size`, and why that assert comes after the checksum check.

> _Your answer:_

### Q3. Back-of-envelope

A full batch is 8,190 transfers. ashi runs 500,000 transfers per second. You want an invariant check that costs 2 µs per call.

1. What fraction of one core does it use if run once per batch? Once per transfer?
2. The VOPR runs 5× slower with the per-transfer check. If you have 1,000 CPU-hours a week for simulation, how many fewer seeds do you test? What does `constants.verify`'s comment say about this trade?

> _Your answer:_

### Q4. What breaks if…

ashi replaces every `assert!` with `debug_assert!` and ships the default Cargo release profile. A bug makes one replica compute a fee as `u64::MAX + 1`. Walk through what happens on that replica, on the cluster (Concept 20), and in the data. Compare with the TigerStyle setup.

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle documents a "hard" 70-line limit but enforces only a 71–72-line red zone. For ashi: should `too_many_lines` at 70 be a CI error from day one, a warning, or a ratchet? How would you implement a ratchet with clippy? Which other TigerStyle rules would you deliberately *not* adopt in Rust, and why?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- `max_latency_ms` → `latency_ms_max` (most significant word first, qualifier last).
- `numRetries` → `retry_count` (snake_case, no abbreviation; "count" says it is 1-based).
- `dst_off` → `target_offset` (no abbreviations; `target` pairs with `source`, same length).
- `timeout` → `timeout_ms` (units in the name).
- Examples in `constants.zig`: `message_size_max`, `clients_max`, `vsr_releases_max`, `grid_scrubber_reads_max`, `journal_slot_count`, `tick_ms`.

**Q2.**
- Bytes read from disk can be wrong because of the hardware: bit rot, misdirected writes (Concept 14). That is an operating error. It must be handled (report the fault, repair from another replica, Concept 25), not crash the replica. Crashing on bad disk data would turn every disk fault into an outage.
- Once `valid_checksum()` passes, the header is exactly what this code wrote. A size bigger than `block_size` could then only come from a bug in the writer, a programmer error, so asserting is right.
- Order matters: before the checksum, the header is untrusted data, and asserting on untrusted data would crash on corruption.
- This is the pair-assertion idea: the writer guarantees zeroed padding; the reader verifies it. A bug on either side is caught.

**Q3.**
1. Per batch: 500,000 / 8,190 ≈ 61 batches/s. 61 × 2 µs ≈ 122 µs per second, about 0.012% of a core. Per transfer: 500,000 × 2 µs = 1 s per second, 100% of a core. The per-transfer version is infeasible in production.
2. A 5× slowdown means 5× fewer seeds in the same budget: you test 20% as many, so 80% fewer. `constants.verify` says tests tolerate up to 5×, but "Slow thorough assertions decrease the overall test coverage", and in the data plane you should "never use O(N) asserts for O(1) computations". So: one O(1) check per batch unconditionally; the per-transfer check only under `verify`, and only if benchmarks show it's costly.

**Q4.**
- `debug_assert!` is compiled out, so no invariant check runs. The default release profile has `overflow-checks = false`, so `u64::MAX + 1` wraps to 0. The fee becomes 0 with no crash.
- If the bug depends on something that differs between replicas (memory layout, timing, a non-deterministic input), replicas now hold different balances. Their state no longer matches, violating Concept 20. Checksums of the stored state diverge. If all replicas share the bug, all of them silently store the wrong fee.
- Either way the wrong balance is durable, replicated and visible to clients. That is a safety violation.
- With TigerStyle (asserts on, overflow traps, `panic = "abort"`), the replica crashes before committing. The cluster loses one replica (a liveness issue) and keeps a correct ledger. The simulator would also have produced a reproducible crash seed during testing.
- Separately: if the overflow came from client input, it should have been a handled `checked_add` with a result code, not a crash.

**Q5.** No single right answer. A strong one:
- Day one, with no code yet, a hard limit is cheapest: `too_many_lines` at 70 as a CI error, with `#[allow(clippy::too_many_lines, reason = "...")]` as the reviewed escape hatch. `allow_attributes_without_reason` forces the reason.
- A ratchet: keep a checked-in list (or count) of allowed exceptions. CI fails if the count grows. That mirrors tidy's "prevent originally small functions from growing" without blocking deliberate long functions such as a big `match` in a protocol handler.
- Rules to adapt or skip:
  - `VSRState` acronyms: follow Rust's `VsrState`.
  - "No `usize`": impossible for indexing; restrict it to wire, disk and config types.
  - Out-pointer in-place init: needs `unsafe`, which ashi forbids.
  - Zig-only bans (`usingnamespace`, `Self = @This()`) have no meaning in Rust; Rust's `Self` is idiomatic.
- Rules where Rust can do *better*: newtypes for index/count/size, and exhaustive `match` with `wildcard_enum_match_arm`.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
