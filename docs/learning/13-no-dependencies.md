# Concept 13: Zero dependencies

**Module:** M2 Memory & execution · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "No Dependencies", "Zig" and "Static Memory Allocation"
- `../tigerbeetle/docs/TIGER_STYLE.md`, sections "Dependencies" and "Tooling"
- `../tigerbeetle/docs/concepts/performance.md` ("built fully from scratch")
- `../tigerbeetle/docs/operating/deploying/README.md` ("single, statically linked binary")
- `../tigerbeetle/docs/internals/HACKING.md` (toolchain bootstrap)
- `../tigerbeetle/zig/download.sh` (pinned compiler)
- `../tigerbeetle/build.zig` (`build_tigerbeetle_executable`, `fetch_objcopy`, `fetch_release`, client `link_libc`)
- `../tigerbeetle/src/stdx/stdx.zig` and modules: `prng.zig`, `flags.zig`, `shell.zig`, `testing/snaptest.zig`, `vendored/aegis.zig`, `io/`
- `../tigerbeetle/src/vsr/checksum.zig` (why the checksum is "set in stone")
- `../tigerbeetle/src/message_pool.zig:134` (alignment-carrying pointer type)
- `../tigerbeetle/src/clients/rust/Cargo.toml`, `../tigerbeetle/src/clients/rust/src/oneshot.rs`

## 1. The problem

A **dependency** is code you ship, or run at build time, that you didn't write. It might be a library, a framework, a code generator or a downloaded tool. For a ledger database, every dependency brings four kinds of risk:

- **Supply chain.** A compromised or hijacked package runs inside the process that holds everyone's balances. Build scripts and code generators run on your build machines, too.
- **Behavior drift.** An upgrade can change performance, memory behavior or, worst of all, *output bytes*. If a hashing library changes its output, every checksum on disk is suddenly "corrupt".
- **Design mismatch.** General-purpose libraries allocate on demand, spawn threads, read the clock and pick random seeds. Each of those quietly breaks Concept 11 (static allocation), Concept 6 (single thread) and Concept 29 (deterministic simulation).
- **Lifetime mismatch.** A database file written today must be readable in 20 years. The libraries you depend on today may be unmaintained in five.

## 2. The obvious solution, and why it fails

The obvious approach is "don't reinvent the wheel": pick best-of-breed packages for the async runtime, serialization, CLI parsing, logging, random numbers, checksums and error handling.

Each pick looks harmless. Together they form a **transitive tree** of packages you never chose directly. A rough, illustrative sketch of the audit burden (run `cargo tree` on a real service to get your own numbers):

```
150 transitive crates × ~4 releases/year each ≈ 600 version bumps/year
600 / ~250 working days ≈ 2–3 upgrades to review, every working day, forever
```

Nobody reviews that. So in practice nobody reviews it.

The design mismatch is the deeper failure. Suppose the hash map crate grows its table on insert, the logging crate allocates a `String` per line, and the RNG crate seeds itself from the OS. Now static allocation and deterministic simulation are *impossible*, not merely harder, because you don't control the code that breaks them.

## 3. What TigerBeetle does

### 3.1 The policy and its rationale

TigerStyle (`docs/TIGER_STYLE.md`, "Dependencies"):

> TigerBeetle has **a "zero dependencies" policy**, apart from the Zig toolchain. Dependencies, in general, inevitably lead to supply chain attacks, safety and performance risk, and slow install times. For foundational infrastructure in particular, the cost of any dependency is further amplified throughout the rest of the stack.

`docs/ARCHITECTURE.md` ("No Dependencies") gives three reasons:

1. **Lifetime.** "The usefulness of dependencies is generally inversely-proportional to the lifetime of the project. For longer lived projects, it makes sense to control more of the underlying moving parts."
2. **Co-design.** Owning the stack "removes temptation to compromise --- static allocation is done throughout the stack because we wrote most of the stack." `docs/concepts/performance.md` says the same: built from scratch "to make sure that all the layers are co-designed for OLTP".
3. **Simplicity as a forcing function.** "Extra complexity can't sneak into the codebase hidden by a nifty API. It turns out that most of the code in the database isn't _that_ hard."

TigerStyle extends the idea to **tools**: "A small standardized toolbox is simpler to operate than an array of specialized instruments". So scripts are written in Zig (`scripts/*.zig`), not shell.

### 3.2 What TigerBeetle *does* depend on

Per `ARCHITECTURE.md`:

- **the Linux kernel API**, in particular io_uring (Concept 12);
- **the Zig compiler**;
- **parts of the Zig standard library**, "for various basic appliances like sorting algorithms or hash maps".

What that looks like in the code:

- **The compiler is pinned exactly.** `zig/download.sh` fetches `ZIG_RELEASE="0.16.0"` and verifies a SHA-256 checksum per platform. The docs build with the downloaded `./zig/zig build`, not a system-wide Zig (`docs/internals/HACKING.md`).
- **The server binary links no libc.** `build_tigerbeetle_executable` (`build.zig:863`) never sets `link_libc`. Zig talks to the Linux kernel directly, which the docs list as a Zig benefit ("direct (glibc-less) bindings to the kernel"). Only the client libraries set `link_libc = true` (`build.zig:1571`), because they are loaded into other languages' processes. The result is "a single, statically linked binary without external dependencies" (`docs/operating/deploying/README.md`).
- **The parts of std it uses** include the io_uring binding (`linux.IoUring`, `src/stdx/io/linux.zig:5`) and unmanaged hash maps (`std.AutoHashMapUnmanaged` in `src/vsr/client_sessions.zig:41`), used in the static-allocation style of Concept 11. Release builds default to `ReleaseSafe` (`build.zig:89`), which keeps bounds and overflow checks on.

### 3.3 `stdx`: their own standard library extensions

`src/stdx/stdx.zig` opens with: "Extensions to the standard library -- things which could have been in std, but aren't." It is roughly 23.6k lines of Zig (about 8.1k of which are the IO backends). Here is what it replaces, and what a typical Rust project would have pulled in instead:

| `stdx` module | What it is | Typical Rust crate it displaces |
|---|---|---|
| `io/` (`linux.zig`, `darwin.zig`, `windows.zig`, `linux_epoll.zig`) | completion-based event loop (Concept 12) | `tokio`, `mio`, `io-uring` |
| `queue.zig`, `stack.zig`, `iops.zig`, `ring_buffer.zig`, `bounded_array.zig`, `bit_set.zig` | intrusive and fixed-capacity containers | `intrusive-collections`, `arrayvec`, `heapless`, `bitvec` |
| `prng.zig` | deterministic PRNG | `rand` |
| `flags.zig` (1,477 lines) | CLI parser enforcing TigerBeetle's CLI rules | `clap` |
| `shell.zig` | "an in-process sh+coreutils combo" for scripts | `xshell`, bash |
| `testing/snaptest.zig` | snapshot tests that "update [their] own source code" | `insta`, `expect-test` |
| `testing/fuzz.zig`, `benchmark/` | fuzz and bench helpers | `proptest`, `criterion` |
| `vendored/aegis.zig` | frozen copy of a std crypto primitive | (see below) |
| `mlock.zig`, `huge_page_allocator.zig` | OS memory controls (Concept 11) | `libc` / `nix` calls |

Two modules show that **even the standard library counts as a dependency** when it touches determinism or on-disk bytes:

- **`vendored/aegis.zig`**: "Vendored from Zig's 0.13.0 standard library to maintain hash stability." TigerBeetle's checksum (`src/vsr/checksum.zig`) is an AEGIS-128L MAC with a zero key, and "As this checksum is stored on disk, it is set in stone and impossible to change." The toolchain has since moved to 0.16.0, while the checksum code stays frozen at 0.13.0.
- **`prng.zig`**: it matches Zig's `std.Random.DefaultPrng`, but TigerBeetle avoids using that directly "to: remove floating point from the API, to ensure determinism; isolate our test suite from stdlib API churn; isolate TigerBeetle from the churn in the PRNG algorithms". If the PRNG changed under them, every VOPR seed (Concept 29) would replay a different history.

### 3.4 Where the edges are

"Zero dependencies" is about the **database's runtime**. Build and release tooling has a few carefully pinned exceptions:

- **Multiversion builds** (Concept 32) download a prebuilt `llvm-objcopy` 18.1.8 from TigerBeetle's own `tigerbeetle/dependencies` GitHub repository, pinned by content hash (`fetch_objcopy`, `build.zig:2868-2913`). They also download past TigerBeetle releases to embed (`fetch_release`, `build.zig:2800-2842`). Those are fetched by URL with `.hash = null`, so they are not hash-pinned in `build.zig`.
- **Clients** live in other ecosystems (Java, .NET, Go, Node, Python, Rust) and carry those toolchains. Even there the habit holds. The Rust client's `Cargo.toml` has **no `[dependencies]` at all**, and it ships its own executor-agnostic completion cell (`src/clients/rust/src/oneshot.rs`: "An embeddable single-producer, single-consumer completion cell. The enclosing allocation owns it and the cell doesn't allocate.").

> ⚠️ **Docs vs code.** `docs/concepts/performance.md` says TigerBeetle "is built fully from scratch, without using any dependencies". The *runtime* claim holds: no third-party libraries, no libc in the server. But the build also fetches a pinned third-party binary (`llvm-objcopy`) for release builds, and the code uses a vendored copy of Zig std's AEGIS. TIGER_STYLE's wording, "apart from the Zig toolchain", plus pinned build tooling, is the precise version.

### 3.5 Zig as the language choice

The language choice follows from static allocation (`ARCHITECTURE.md`, "Zig"): no garbage collector, because "it makes it much harder to guarantee that no allocation happens". That leaves few options: "Of the languages that remain, Zig makes the most sense, although Rust is a close contender."

**TigerBeetle's case for Zig**, quoted from the docs:

| Argument | Detail |
|---|---|
| Safety parity where it matters | "Both Zig and Rust provide spatial memory safety" (bounds checks). |
| Rust's extra safety matters less here | "Rust has better temporal and thread safety but static allocation and single-threaded execution reduce the relative importance of these benefits." |
| Correctness comes from testing | "Requiring a comprehensive testing strategy leaves little space for bugs to escape testing but be caught by the Rust-style type system." (Concepts 29–31) |
| Expressivity per unit of complexity | "Zig is a DSL for machine code." `comptime` lets you "_directly_ express what you want the computer to do", at "the cost of missing declaration-site interfaces". |
| Layout control | "excellent control over layout, alignment and padding. Alignment-carrying pointer types prevent subtle errors". Example: `buffer: *align(constants.sector_size) [constants.message_size_max]u8` (`src/message_pool.zig:134`); the type itself guarantees a sector-aligned buffer for Direct IO (Concept 14). |
| Allocator-explicit collections | Allocators are passed to methods, "a perfect fit" for static allocation. |
| Low dependency count | "Cross compilation that works and direct (glibc-less) bindings to the kernel". |

**The fair case for Rust**, which is why ashi chose it:

| Argument | Detail |
|---|---|
| Stability | Rust has kept a backwards-compatibility promise since 1.0 (2015), with editions for breaking changes. Zig is pre-1.0. TigerBeetle pins 0.16.0 and pays for upgrades, and vendoring AEGIS and owning the PRNG are partly defenses against std churn. |
| The borrow checker still helps single-threaded code | It catches aliasing bugs such as iterator invalidation, or a reference into a pool slot that was just released. "Single-threaded" does not mean "no temporal bugs". |
| Tooling and people | `cargo`, `clippy`, Miri (an interpreter that detects undefined behavior), `rust-analyzer`, a large hiring pool, and safety-qualified toolchains (Ferrocene). |
| Layout is available | `#[repr(C, align(N))]`, `const` assertions on `size_of`, `#![no_std]` if needed. |

Rust's honest weak spots for *this* design:

- **Alignment does not travel with slice pointers.** You need newtypes like `#[repr(align(4096))] struct Sector([u8; 4096])`.
- **`const fn` and const generics are weaker than `comptime`.**
- **std collections hide the allocator** (Concept 11).
- **Pointer-heavy intrusive designs push you toward `unsafe` or indices** (Concept 12).
- **Overflow checks are off in release by default.** TigerBeetle's own Rust client `Cargo.toml` says it is "*strongly* recommended" to enable `overflow-checks = true`, "because the nature of accounting makes overflow errors catastrophic".

## 4. What it means for ashi

### The crates.io culture problem

Rust's ecosystem favors many small crates. Some risks are specific to it:

- **`build.rs` and proc macros run arbitrary code at build time** on developer and CI machines.
- **Feature unification.** Features are unified across the whole graph, so one crate enabling a feature turns it on for everyone.
- **Default features** pull in extras unless you opt out with `default-features = false`.
- **MSRV churn.** A patch release can raise the minimum supported Rust version and break the build.
- **Std gaps.** Rust's std has no RNG, no CLI parser, no io_uring and no checksum, so the pull toward crates starts on day one.

Rust std also links a **C library** (glibc or musl) on Linux. TigerBeetle's server has none. If that matters to you, build for `x86_64-unknown-linux-musl` for a static binary. Going fully libc-free means `#![no_std]` plus raw syscalls (or `rustix`'s raw-Linux backend), which is a big step.

### Tools to keep the tree honest

| Tool | What it does |
|---|---|
| `Cargo.lock` + `--locked` / `--frozen` / `--offline` in CI | builds exactly the reviewed graph |
| `rust-toolchain.toml` | pins the compiler, like `zig/download.sh` |
| `cargo vendor` + source replacement in `.cargo/config.toml` | dependency source in-repo; builds never touch the network |
| `cargo-deny` | policy gate: `[bans]` (deny-list, duplicate versions), `[licenses]`, `[advisories]` (RustSec), `[sources]` (only crates.io / allowed git) |
| `cargo-audit` | RustSec vulnerability check |
| `cargo-vet` | records *who audited which version*; can import other organizations' audits |
| `cargo tree -d`, `cargo tree -e features` | find duplicates and feature bloat |
| `cargo-geiger` | counts `unsafe` in your tree |
| `cargo-machete` / `cargo-udeps` | find unused dependencies |

A `deny.toml` that matches a "near-zero" policy can deny everything by default and allow-list by name (`[bans] allow = [...]`). That makes adding a dependency a reviewed, visible diff.

### Which dependencies are defensible for a ledger

| Tier | Examples | Reasoning |
|---|---|---|
| Toolchain | `rustc`, `core`, `alloc`, `std` | the equivalent of TigerBeetle's "Zig compiler + parts of std"; pin the version |
| Thin OS boundary | `libc` (maintained by the rust-lang org); maybe `io-uring` (thin bindings, tokio-rs org) or `rustix` | wraps the kernel, not policy. Alternative: hand-write the ~few hundred lines of io_uring setup and opcode structs you actually need |
| Dev-only | `proptest`, `criterion` | never shipped, but still build-time code execution. Your simulator needs your own PRNG anyway |
| Avoid in the core | async runtimes, `serde` for on-disk or wire formats, checksum/hash crates, `rand`, logging frameworks that allocate, `anyhow` | each one breaks static allocation, determinism, or "bytes set in stone" |
| Judgment call | `zerocopy` / `bytemuck` for safe casting of `#[repr(C)]` structs (Concept 9) | well audited and removes `unsafe`, but adds proc macros. The alternative is a small, audited set of `unsafe` casts guarded by `const` size/align assertions |

**Own the bytes.** Anything whose output is persisted or compared across replicas must be owned or vendored, with test vectors checked into the repo:

- the checksum (AEGIS-128L can be written with `core::arch` AES intrinsics);
- the PRNG;
- the serialization layout.

This is TigerBeetle's AEGIS lesson.

### Pitfalls specific to Rust

- "Zero dependencies" is easy to lose by accident: one `cargo add` of a convenience crate brings 20 more. Gate additions in CI with `cargo-deny`.
- Vendoring without review is just slower supply-chain risk. Pair `cargo vendor` with `cargo-vet` records.
- Writing your own crates is not free either. Budget for fuzzing them (Concept 31) to the same standard as the core.

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adapt**. Near-zero *runtime* dependencies (at most `libc` and possibly `io-uring`), dev dependencies allowed behind a `cargo-deny` allow-list, and anything that defines persisted bytes or randomness owned outright.

## 5. Drill questions

### Q1. Recall

What three things does `ARCHITECTURE.md` say TigerBeetle depends on? Name two places where TigerBeetle deliberately avoids even the Zig standard library, and the reason for each.

> _Your answer:_

### Q2. Mechanism

Explain the claim "static allocation is done throughout the stack because we wrote most of the stack." Give a concrete example of a typical third-party library that would make Concept 11 or Concept 29 impossible to uphold.

> _Your answer:_

### Q3. Back-of-envelope

ashi's prototype has 120 transitive crates. Each publishes about 5 releases a year. Your team can carefully review about 5 crate upgrades per week.

- Can you keep up with every upgrade?
- What fraction of upgrades go unreviewed?
- What does that imply for your policy?

> _Your answer:_

### Q4. What breaks if…

TigerBeetle had used Zig's std AEGIS directly, and Zig 0.15 changed its output for some input lengths. A cluster upgrades one replica to a binary built with the new compiler. What breaks: on that replica's disk, between replicas, and in the hash chain (Concept 15)?

> _Your answer:_

### Q5. ashi design (open-ended)

Write ashi's dependency policy in five bullet points:

- What is allowed at runtime?
- What is allowed in dev/test?
- How do new dependencies get approved?
- Which components must be owned outright?
- How do you pin the toolchain?

Then name the single crate you would fight hardest to keep, and argue for it.

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** The three dependencies:
- the Linux kernel API (especially io_uring);
- the Zig compiler;
- parts of the Zig std (sorting, hash maps, and in practice also the io_uring binding).

Std is avoided in two places:
- **AEGIS is vendored** from Zig 0.13.0 "to maintain hash stability". The checksum is stored on disk and "set in stone".
- **The PRNG is TigerBeetle's own** (`stdx/prng.zig`), to remove floating point for determinism and to isolate tests and seeds from std API and algorithm churn.

Other acceptable answers: `flags.zig` instead of any CLI library, `shell.zig` instead of shell scripts, `snaptest` for testing.

**Q2.** Static allocation needs every allocation in the process to happen at init, and every collection to be bounded. You can only guarantee that for code whose allocation behavior you control.

Examples that break it:
- a JSON or serde-style decoder that allocates `String`s per field;
- a logging crate that formats into a heap `String` per line, or spawns a background thread;
- an async runtime that boxes each spawned task;
- an RNG seeded from the OS, which breaks seed replay for Concept 29;
- a hash map that grows on insert.

Because TigerBeetle wrote the IO loop, the containers, the PRNG and the CLI parser, each could be designed around "allocate at init, never again" and "same seed, same history".

**Q3.**
- 120 × 5 = **600 upgrades/year ≈ 11.5/week**. Capacity is 5/week, so **you can't keep up**.
- About 57% of upgrades go unreviewed ((11.5 − 5) / 11.5).
- Options:
  - cut the tree (the biggest lever);
  - upgrade in deliberate batches and accept lag, using `cargo-audit` alerts to force security upgrades;
  - import other organizations' audits with `cargo-vet`.

The math is the argument for TigerBeetle's policy: the review cost scales with the tree, while the benefit of each small crate is small.

**Q4.**
- **On disk:** the upgraded replica recomputes checksums differently, so every existing block, superblock copy and WAL entry looks corrupt to it. It would try to repair everything from peers (Concepts 24–25), or refuse to start.
- **Between replicas:** messages carry checksums too ("validate network messages before casting raw bytes", `checksum.zig`), so the new replica rejects valid messages from old replicas and the reverse. The cluster can't make progress across the version split.
- **Hash chain:** each prepare's header includes the checksum of its parent. Recomputed parent checksums wouldn't match, so the log looks forked.

Multiversion upgrades (Concept 32) assume old and new binaries agree on bytes. A silent std change would break that assumption cluster-wide. Vendoring makes the bytes independent of the compiler version.

**Q5.** A sample policy:
1. **Runtime:** only `std`/`core`/`alloc`, plus `libc` (and `io-uring` if you don't hand-roll it). Each is pinned exactly.
2. **Dev/test:** allowed, behind a `cargo-deny` allow-list. Nothing from dev crates leaks into persisted formats.
3. **Approval:** a new dependency needs a written justification, a `cargo-vet` audit entry, a license check, and `default-features = false`.
4. **Owned outright:** the checksum, PRNG, wire/disk layout, event loop, pools and containers, CLI parsing, and the simulator.
5. **Toolchain:** `rust-toolchain.toml` with an exact version, plus `Cargo.lock` and `--locked` in CI. Vendor sources for releases.

The "fight hardest" crate is your call. A strong pick is `libc`: it's maintained by the Rust project, a thin layer over the kernel, and rewriting it is pure risk with no design benefit. Another is `io-uring`: it saves error-prone unsafe setup code, but it's small enough to replace if it ever drifts.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
