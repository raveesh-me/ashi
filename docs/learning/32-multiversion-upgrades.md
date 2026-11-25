# Concept 32: Upgrades with multiversion binaries

**Module:** M6 Operations · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/internals/upgrades.md` (all sections: Building, Bootstrapping, Monitoring, Executing)
- `../tigerbeetle/docs/operating/upgrading.md`, sections "Planning for upgrades" and "Upgrading binary-based installations"
- `../tigerbeetle/docs/concepts/safety.md:121`, the "Human Fallibility" bullet on atomic upgrades
- `../tigerbeetle/src/multiversion.zig`: `Multiversion`, `Release`, `MultiversionHeader`, `MultiversionOS`, `parse_elf`
- `../tigerbeetle/src/build_multiversion.zig`: `build_multiversion_single_arch`, `build_multiversion_body`, `self_check`
- `../tigerbeetle/src/vsr/replica.zig`: `on_upgrade_timeout`, `execute_op_upgrade`, `release_for_next_checkpoint`, `release_transition`
- `../tigerbeetle/src/vsr.zig`: `quorums`, `Operation.upgrade`, `UpgradeRequest`, `Checkpoint.trigger_for_checkpoint`
- `../tigerbeetle/src/vsr/superblock.zig:383-388` (`CheckpointState.release`)
- `../tigerbeetle/src/constants.zig:87-113`, `../tigerbeetle/src/config.zig:141-168`, `../tigerbeetle/build.zig:10-17`

## 1. The problem

A database runs for years. Its code changes every week. So at some point a running cluster has to switch from code version `v1` to `v2` without losing data and with as little downtime as possible.

For a replicated state machine (see Concept 20) this is harder than for a web server:

- Every replica applies the same log of operations (ops) and must reach **the same state**, byte for byte.
- That only works if every replica runs **the same state-machine logic for the same op**.
- `v2` might legitimately compute a different result for the same transfer. A bug fix, a new validation rule or a new result code all do that.
- So if replica A executes op 1000 with `v1` and replica B executes op 1000 with `v2`, their balances can diverge. That is silent corruption in a ledger.

There's a second, sneakier version of the same hazard. A replica can execute op 1000 with `v1`, crash, restart with the new `v2` binary, and **replay** op 1000 from the write-ahead log (WAL). Now one replica has run the same op under two codebases.

Operators also make mistakes. They upgrade replicas in the wrong order, skip versions, or restart a container mid-upgrade. A financial database must survive all of that.

## 2. The obvious solution, and why it fails

**Option A: rolling restart.** Stop replica 1, swap the binary, start it. Repeat for each replica.

- During the roll, the cluster runs a **mix of versions**. The primary on `v1` and a backup on `v2` both commit op N. If `v1` and `v2` disagree on op N's result, the replicas diverge.
- Nothing pins the switch to a particular op. The switch point is "whenever the operator got to that box".
- A replica that crashed after the swap restarts on `v2` and replays ops that `v1` already executed.

**Option B: stop the world.** Stop all replicas, swap all binaries, start all.

- This is correct, but the cluster is down for the whole procedure, including any WAL replay.
- It needs external coordination: a runbook, a human, or an orchestrator that touches every node at once.
- It still fails the "skip versions" case. Going from 0.15.5 to 0.15.20 may need intermediate data-format migrations that only the versions in between know how to do.

**Option C: one binary with `if version >= X` branches everywhere.** You keep all old behaviour forever inside one codebase.

- The state machine becomes a museum of past bugs. Every past result code and every past validation rule must stay, byte-exact, behind a version flag.
- It's very easy to miss one branch. The miss is silent until replicas disagree.

What you actually want: **every op is executed by exactly one version of the code, on every replica, including during replay**, and the cluster agrees on the switch point through consensus.

## 3. What TigerBeetle does

The safety doc states the goal directly (`docs/concepts/safety.md:121`):

> Upgrades are automatic and atomic, guaranteeing that each transfer is applied by only a single version of code.

The design has three parts:

1. **Package:** one binary bundles the current release plus a few past releases.
2. **Detect:** a running replica notices that its binary file changed and advertises the new releases it now contains.
3. **Switch:** the primary proposes an upgrade through consensus. All replicas switch at the **same checkpoint**, by re-executing (`exec`) into the new code.

### 3.1 Releases as numbers

A release is a `u32` holding a `major.minor.patch` triple (`src/multiversion.zig:177`, `:255`):

```zig
pub const ReleaseTriple = extern struct {
    patch: u8,
    minor: u8,
    major: u16,
    ...
};
```

Because `major` is the most significant field, comparing two `Release.value`s as integers orders them correctly. Major `65535` is reserved for test builds. A replica refuses to open a real cluster (`cluster != 0`) with such a build (`src/vsr/replica.zig:744-748`).

### 3.2 The multiversion binary layout

A multiversion binary is a normal ELF (Linux), PE (Windows) or Mach-O (macOS) executable with **two extra sections**. They are marked `noload`, so the loader never maps them into memory (`src/build_multiversion.zig:159-167`):

```
--add-section .tb_mvb={body}
--set-section-flags .tb_mvb=contents,noload,readonly
--add-section .tb_mvh={header_zero}
--set-section-flags .tb_mvh=contents,noload,readonly
```

```
tigerbeetle (ELF)
┌──────────────────────────────────────────┐
│ ELF header, program headers              │
│ .text .rodata .data ...                  │  ← the CURRENT release; this is what runs
├──────────────────────────────────────────┤
│ .tb_mvb  "MultiVersion Body"             │  second-to-last section (enforced)
│   ┌────────────┬────────────┬─────────┐  │
│   │ past rel 1 │ past rel 2 │  ...    │  │  complete older binaries, concatenated
│   └────────────┴────────────┴─────────┘  │
├──────────────────────────────────────────┤
│ .tb_mvh  "MultiVersion Header" 8192 B    │  last section (enforced)
│   checksum_header, checksum_binary_...   │
│   current_release, current_checksum      │
│   past.releases[63] .offsets[63]         │
│   past.sizes[63] .checksums[63] .flags   │
│   release_client_min, git_commit, ...    │
└──────────────────────────────────────────┘
```

`parse_elf` enforces the section order: the body must be second-to-last and the header must be last (`src/multiversion.zig:1837-1857`). The short names fit PE's 8-character section-name limit (`upgrades.md`, footnote 3). On macOS the sections are extra entries in a fat (universal) binary, tagged with unused CPU type IDs (`src/multiversion.zig:85`).

The header is a fixed-size `extern struct`, checked at compile time (`src/multiversion.zig:576-586`):

```zig
assert(constants.vsr_releases_max == 64);
assert(PastReleases.past_releases_max == 63);
assert(@sizeOf(MultiversionHeader) == 8192);
assert(@offsetOf(MultiversionHeader, "checksum_header") == 0);
```

Key fields (`src/multiversion.zig:308-500`):

- `checksum_header`: covers the rest of the header.
- `checksum_binary_without_header`: covers **the whole file** with the `.tb_mvh` section zeroed. You can't checksum a file that contains its own checksum, so the build inserts a zeroed header, hashes, then patches the real header in (`src/build_multiversion.zig:157-216`).
- `past.checksums[i]`: a checksum of each embedded past binary. It is verified before that binary is ever executed.
- `schema_version`: lets the header format itself change later through a transitional release.
- `reserved: [4744]u8`: room to add fields without a schema bump.

This is Concept 15 (checksums everywhere) applied to the executable itself. A corrupt binary is caught **before** `exec`, not after.

Relevant limits:

| Constant | Value | Where |
|---|---|---|
| `vsr_releases_max` | `64` | `src/config.zig:168` |
| `multiversion_binary_platform_size_max` | `64 * MiB` | `src/config.zig:141` |
| `multiversion_binary_size_max` | platform max `* 2 * 2` = 256 MiB (macOS universal ×2, debug ×2) | `src/constants.zig:104-105` |
| `multiversion_poll_interval` | `.ms(1000)` | `src/config.zig:142` |
| `release_client_min` (current tree) | `"0.16.4"` | `build.zig:17` |

### 3.3 Which past releases get bundled

The header has room for 63 past releases. The build actually packs far fewer (`src/build_multiversion.zig:525-538`):

```zig
// You can have as many releases as you want, as long as it's 5 or less.
// This is made up of:
// * up to 3 releases from the old past pack (extracted from the release downloaded),
// * 1 old current release (extracted from the release downloaded),
// * 1 current release (that was just built).
...
const past_count: u32 = if (options.debug) 0 else @min(3, header.past.count);
```

The release script does **not** rebuild old releases from source. It downloads the previous GitHub release, cuts its embedded pack out with `llvm-objcopy`, and checks every piece against the checksums in the old header (`src/build_multiversion.zig:512-517`, `:572-576`). So each release carries a sliding window: itself, the release before it, and up to three before that.

That window is where the operator-facing rule in `docs/operating/upgrading.md` comes from: each release names "the oldest release that can be upgraded from". If your running version has fallen out of the window, you must upgrade in several hops.

Before publishing, `self_check` runs `tigerbeetle multiversion <new binary>` with the new binary **and with every past binary**, so old code proves it can parse the new header (`src/build_multiversion.zig:787-800`).

### 3.4 Visiting every release on the way

Each past release has a `visit` flag. The doc comment says (`src/multiversion.zig:310-312`):

```zig
/// Normally release upgrades are allowed to skip to the latest. If a corresponding release
/// is set to true here, it must be visited on the way to the newest release.
visit: bool,
```

`advertisable(from_release)` lists every past release, then stops at the first newer release that has `visit` set (`src/multiversion.zig:556-575`). But when the build moves the "old current" release into the past pack, it always sets that flag (`src/build_multiversion.zig:590-595`):

```zig
// Visit https://github.com/tigerbeetle/tigerbeetle/pull/2181.
old_current_release_flags.visit = true;
```

Every release enters the pack as an "old current" release, so in practice every bundled past release has `visit = true`. A cluster on 0.16.1 with a binary bundling 0.16.2..0.16.5 upgrades **one release at a time**: 0.16.2, then 0.16.3, and so on. Each hop runs that release's own checkpoint and data migrations. The operator still does one binary swap; the cluster walks the chain by itself.

> ⚠️ **Docs vs code.** `upgrades.md` says multiversion binaries let "migrations over a range happen easily without having to manually jump from version to version", and the `visit` doc comment says upgrades "normally" skip to the latest. Both are true for the *operator*. The code shows the *cluster* still steps through every bundled release, because `build_multiversion.zig:594` forces `visit = true` on each one. The reason is in PR #2181, which I did not read; it is not in the clone.

### 3.5 Detecting a new binary

`MultiversionOS` (`src/multiversion.zig:663`) owns this. It's a small explicit state machine (see Concept 12) with stages `init → source_stat → source_open → source_read → target_update → ready | err`.

- A timer fires every `multiversion_poll_interval` (1000 ms) worth of ticks (`src/multiversion.zig:859-863`).
- It runs `statx` on its own executable path, zeroes `atime`, and compares the result byte-for-byte with the previous one (`src/multiversion.zig:1006-1040`).
- If anything changed, it reads the whole file, finds the sections, verifies both checksums, and runs two safety checks (`target_update`, `src/multiversion.zig:1215-1229`):
  1. The new binary must still contain the release that is running right now.
  2. Every newer release already advertised must still be present (`error.NotSuperset`).
- If both pass, it copies the bytes into a **memfd** (an anonymous in-memory file, created by `memfd_create`) and updates `releases_bundled`.

Why copy into memory? The file on disk can change again at any time. An operator might be halfway through `mv`. The memfd is a frozen, verified snapshot that matches exactly the release list the replica is advertising. On macOS and Windows a temp file plays this role. Polling itself is Linux-only (`src/tigerbeetle/main.zig:477-486`).

### 3.6 Agreeing to upgrade

Replicas advertise their bundled releases in every **ping**. The ping body holds up to `vsr_releases_max` releases (`src/vsr/replica.zig:3596-3610`). Each replica records what others can run in `upgrade_targets` (`src/vsr/replica.zig:379`, `:1865-1886`).

The primary runs `on_upgrade_timeout` (`src/vsr/replica.zig:3990`). It picks the newest release that enough replicas can run. "Enough" is the upgrade quorum (`src/vsr.zig:1254-1261`):

```zig
// A majority quorum (i.e. `max(quorum_replication, quorum_view_change)`) is required
// to ensure that the upgraded cluster can both commit and view-change.
//
// However, we farther require that all replicas can upgrade. ...
const quorum_upgrade = replica_count;
```

So **every** active replica must have the new binary before anything happens. Then the primary sends itself a request with `operation = .upgrade` (`vsr.Operation.upgrade = 5`, `src/vsr.zig:317`). The body is a 16-byte `UpgradeRequest { release, reserved: [12]u8 }` (`src/vsr.zig:688-697`).

That request goes through normal consensus. It is a log entry like any transfer batch. When it commits, `execute_op_upgrade` sets `upgrade_release = R` (`src/vsr/replica.zig:5647-5711`). The decision "upgrade to R" now lives in the replicated log, so every replica learns it at the same op.

### 3.7 Switching at a checkpoint

A **checkpoint** (see Concept 17) is a durable snapshot of state at some op. TigerBeetle doesn't write it at that op. It writes it at a later **trigger** op: `trigger = checkpoint + lsm_compaction_ops` (`src/vsr.zig:1678-1686`). Ops between the checkpoint and the trigger are the "last bar". They stay in the WAL and get **replayed** after a restart.

In production, `lsm_compaction_ops = 32` and `journal_slot_count = 1024` (`src/config.zig:158`, `:164`). So `vsr_checkpoint_ops = 1024 − 32 − 32·⌈16/32⌉ = 960` (`src/constants.zig:40-42`).

The last bar is exactly where the replay hazard from section 1 lives. TigerBeetle's fix is in `release_for_next_checkpoint` (`src/vsr/replica.zig:10948-10980`):

```zig
// Only allow the next checkpoint's release to advance if the entire last bar
// preceding the checkpoint trigger consists of operation=upgrade.
//
// Otherwise we would risk the following:
// 1. Execute op=X in the state machine on version v1.
// 2. Upgrade, checkpoint, restart.
// 3. Replay op=X when recovering from checkpoint on v2.
// If v1 and v2 produce different results when executing op=X, then an assertion
// will trip (as v2's reply doesn't match v1's in the client sessions).
```

While `upgrade_release` is set:

- The primary **ignores client requests** (`src/vsr/replica.zig:6500-6508`), so no new transfers enter the log.
- After each commit it sends itself another `upgrade` op (`src/vsr/replica.zig:4955-4962`).
- An `upgrade` op is a no-op for the state machine. `v1` and `v2` "execute" it identically.

Once all 32 ops of the last bar are `upgrade` ops, the next checkpoint is written with `CheckpointState.release = R` (`src/vsr/replica.zig:5256`). The superblock documents the invariant (`src/vsr/superblock.zig:383-388`):

```zig
/// All prepares between `CheckpointState.commit_min` (i.e. `op_checkpoint`) and
/// `trigger_for_checkpoint(checkpoint_after(commit_min))` must be executed by this release.
/// (Prepares with `operation=upgrade` are the exception – upgrades in the last
/// `lsm_compaction_ops` before a checkpoint trigger may be replayed by a different release.
release: vsr.Release,
```

```
op:  ... C ...............................  C' | C'+1 ............ C'+32 = trigger
         │                                     │ │◄── last bar: all operation=upgrade ──►│
         │◄──── executed by v1 only ─────────►│ │ executed by v1, replayed by v2: no-ops │
                                                                                         │
     checkpoint C' written at trigger with CheckpointState.release = v2 ─────────────────┘
     ops after the trigger: executed by v2 only
```

When that checkpoint is durable, the replica calls `release_transition` (`src/vsr/replica.zig:4531-4537`, `:10909`), which calls `multiversion.release_execute(R)`. On Linux that ends in `execveat(memfd, "", args, envp, AT_EMPTY_PATH)` (`src/multiversion.zig:1445-1456`). The process image is replaced; the data file stays.

### 3.8 Starting up: the newest code decides

On startup, the replica reads `superblock.working.vsr_state.checkpoint.release` and compares it with its own release (`src/vsr/replica.zig:740-753`). If they differ, it calls `release_transition` before opening the journal.

- **Binary newer than the checkpoint** (operator swapped the file, then the box rebooted): it **downgrades**. `exec_release` copies the matching past binary out of `.tb_mvb` to the start of the memfd, truncates, re-checksums, and execs it (`src/multiversion.zig:1358-1443`).
- **Binary older than the checkpoint:** `release_execute` panics with "release not available", telling the operator to upgrade the binary (`src/multiversion.zig:1280-1291`).

This is the docs' "key point" (`upgrades.md`): "the newest version is always what starts up and determines what version to run." It's why Docker works. The container restarts on the new image, discovers the checkpoint is older, re-execs into the old embedded release, and waits for the cluster to agree on the upgrade.

### 3.9 The whole flow

```
operator: mv new binary over /usr/bin/tigerbeetle on each replica (any order)
   │
   ▼
each replica: statx every 1000 ms ─► changed ─► read, verify checksums,
              copy to memfd, releases_bundled = [.., v2]
   │
   ▼
pings carry releases_bundled ─► primary's upgrade_targets
   │
   ▼
primary on_upgrade_timeout: v2 on all replica_count replicas?
   │ yes
   ▼
prepare operation=upgrade{v2} ─► consensus ─► commit ─► upgrade_release = v2
   │  (client requests ignored; primary keeps preparing upgrade no-ops)
   ▼
last bar before the trigger is all upgrade ops ─► checkpoint with release = v2
   │
   ▼
checkpoint durable ─► release_transition ─► execveat(memfd) ─► v2 opens, replays no-ops
```

### 3.10 Clients

The replica enforces a window of client versions (`src/vsr/replica.zig:6299-6330`):

- client release `< release_client_min`: evicted with `client_release_too_low`;
- client release `> release` (newer than the replica): evicted with `client_release_too_high`.

So the procedure is **replicas first, then clients**. Old clients keep working as long as they're inside the new release's window. The state machine supports old clients through explicit, separate operation numbers such as `deprecated_create_accounts_unbatched` (`src/tigerbeetle.zig:686`), never through a hidden version argument (`src/state_machine.zig:1061-1062`):

```zig
// NB: This function should never accept `client_release` as an argument.
// Any public API changes must be introduced explicitly as a new `operation` number.
```

That keeps execution a pure function of `(state, op)`, which is what determinism needs (Concept 20).

> ⚠️ **Docs vs code (comment).** `src/vsr/replica.zig:267` lists the invariant `release_client_min ≥ release`. The code asserts the opposite, `options.release.value >= options.release_client_min.value` (`src/vsr/replica.zig:659`, also `src/config.zig:292`). The assert is right; the comment is a typo.

### 3.11 Operational procedure

From `docs/operating/upgrading.md`:

1. Check the target release's notes: the oldest release it upgrades from, and its oldest supported client.
2. On each replica, in any order, put the new binary on the same file system and `mv` it into place. A `mv` within one file system is atomic, so the poller never sees a half-written file.
3. Do nothing else. Replicas detect the binary and the primary coordinates the switch. (Exception: from the epoch release 0.15.3, which can't read its own binary, you restart manually.)
4. Expect a short unavailability; the docs say "on the order of 5 seconds". Clients retry internally.
5. Upgrade the client libraries.

Back-of-envelope for step 4 (my estimate, not from the docs): upgrade no-ops are committed one at a time; `execute_op_upgrade` asserts a single prepare in the queue (`src/vsr/replica.zig:5691-5695`). If the upgrade starts just after a trigger, up to ~960 + 32 serial no-ops must commit before the next one. At 1–5 ms each, that's roughly 1–5 seconds, plus the restart. That matches the docs' figure.

### 3.12 The simulator tests upgrades too

`Multiversion` is an interface with three implementations: `MultiversionOS`, a single-release stub, and the VOPR's (`src/multiversion.zig:18-31`). The VOPR runs three fake releases, 0.0.1 to 0.0.3 (`src/vopr.zig:43-56`). It upgrades replicas at random as they restart (`src/vopr.zig:1543-1551`, `:1657`), so crashes during upgrades get explored (see Concept 29). In tests, `release_execute` returns, and the cluster re-opens the replica in-process instead of calling `exec` (`src/testing/cluster.zig:546-550`).

## 4. What it means for ashi

The hard part is not the packaging. It's the consensus rule in 3.7: **an op is executed by exactly one release, and the switch happens at a checkpoint whose last bar is no-ops.** ashi needs that from day one, even before it can ship a second version, because the checkpoint format must carry a `release` field.

### Rust building blocks

- **Version type:** `#[repr(transparent)] struct Release(u32)` with `major: u16` in the high bits, so `Ord` on the raw `u32` is correct. Add `const _: () = assert!(size_of::<Release>() == 4);`.
- **Header:** `#[repr(C)]` struct with explicit padding, `zerocopy::{FromBytes, IntoBytes, KnownLayout, Immutable}` (or `bytemuck::Pod`), and a const assert that the size is 8192. No serde: the format must be stable across compiler versions.
- **Embedding past binaries:** two options.
  - `include_bytes!` in a `build.rs`-generated module. Simple, but the past binaries become part of `.rodata` and get **mapped into memory** on every run. TigerBeetle avoids that with `noload` sections.
  - Post-link step, like TigerBeetle: `llvm-objcopy --add-section .ashi_mvb=... --set-section-flags .ashi_mvb=contents,noload,readonly`. Read sections at runtime with the `object` crate, or parse ELF section headers by hand (~100 lines). This keeps RSS flat and matches TigerBeetle's checksum-the-whole-file trick.
- **Re-exec:** `rustix::fs::memfd_create` + `rustix::runtime::execveat` (or `libc::syscall(SYS_execveat, ...)`) with `AT_EMPTY_PATH`. `std::os::unix::process::CommandExt::exec` only takes a path, so it can't exec a memfd directly; `/proc/self/fd/N` is a fallback path. Remember `MFD_CLOEXEC` and an `exec` that never returns (`-> !`).
- **Polling:** `statx` through your own IO layer (Concept 12), not a separate thread or `notify` crate. inotify misses the "replaced via `mv` onto the watched path" case unless you watch the directory.
- **`unsafe`:** ashi's workspace has `unsafe_code = "forbid"`. `execveat` and memfd need `unsafe` or a crate that wraps it (`rustix` exposes safe wrappers for both). Isolate it in one small crate with its own lint override.

### The alternative: versioned dispatch inside one binary

Instead of bundling old binaries, ship one binary whose state machine is `match checkpoint_release { V1 => sm_v1::execute(..), V2 => sm_v2::execute(..) }`.

- **Pro:** no exec, no objcopy, no memfd, works on every OS, upgrade is a function-pointer swap at the checkpoint.
- **Con:** every module that affects state must be versioned: state machine, LSM compaction, grid layout, checkpoint encoding. Physical determinism (Concept 20) means compaction output bytes must match too. Old code paths live in the tree forever and must keep compiling against new internals. That's option C from section 2, with discipline.
- **Con:** a bug in "shared" code that both versions call breaks both versions at once. Separate binaries isolate that.
- A middle ground: versioned dispatch only for the state machine's pure `execute` functions, with whole-binary multiversion for everything else.

### Pitfalls specific to Rust

- **Hidden nondeterminism between builds.** `HashMap` with `RandomState`, iteration order, float formatting and `std` sort stability changes across toolchains can all change outputs. Pin the toolchain per release (`rust-toolchain.toml`) and never let a `HashMap` iteration order reach state.
- **Reproducible builds.** TigerBeetle checksums the past binary and compares it to `current_checksum`. If ashi wants to re-derive old binaries, `cargo` builds must be reproducible (`--remap-path-prefix`, fixed `SOURCE_DATE_EPOCH`, no build-time timestamps). Downloading and checksumming the previous release is simpler.
- **Binary size.** Rust release binaries with debug info are large. Five bundled releases at 20–40 MB each is real. Strip, or keep debug info out of the pack.
- **Async runtimes don't survive exec.** If ashi used tokio, every thread and fd must be torn down cleanly before `execveat`. A single-threaded explicit event loop makes "stop at a checkpoint, then exec" trivial.

### Complexity budget

Whole-binary multiversion costs roughly: a header format, a build/release step, a poller, an exec path per OS, a startup downgrade path, and the consensus `upgrade` operation. The consensus part is mandatory either way. The packaging part can wait until ashi has a second release, as long as the checkpoint stores `release` from the first one.

**ashi stance:** _to be decided once the drill is done_

Suggestion: **adopt** the consensus rule (upgrade op, all-replica quorum, checkpoint `release`, no-op last bar) from day one; **adapt** the packaging, starting with a Linux-only post-link section plus `execveat`, since that's the only correct story for "operator swaps the binary and the box reboots".

## 5. Drill questions

### Q1. Recall

What two sections does a multiversion binary add, which one must come last, and why are both marked `noload`?

> _Your answer:_

### Q2. Mechanism

Explain why the cluster refuses to raise `CheckpointState.release` until the **entire** last bar before the checkpoint trigger is `operation=upgrade`. Walk through what goes wrong if a single real transfer batch sits in that bar.

> _Your answer:_

### Q3. Back-of-envelope

A 3-replica production cluster starts an upgrade right after a checkpoint trigger. `lsm_compaction_ops = 32`, `vsr_checkpoint_ops = 960`. Upgrade no-ops commit one at a time, and each takes about 3 ms (replicate + WAL fsync). Roughly how long are client requests ignored? What would change if TigerBeetle allowed an upgrade to start mid-interval and checkpoint early?

> _Your answer:_

### Q4. What breaks if…

…`quorum_upgrade` were a simple majority (2 of 3) instead of `replica_count`? Describe what happens to the third replica, which doesn't have the new binary, once the other two checkpoint on `v2`.

> _Your answer:_

### Q5. ashi design (open-ended)

ashi must choose between whole-binary multiversion (bundle + `execveat`) and versioned dispatch inside one binary. Which modules would you have to version in the second approach to keep **physical** determinism (Concept 20)? Given that, which approach would you pick for ashi's first two years, and what must the very first ashi release store on disk to keep both options open?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.** `.tb_mvb` (body: the concatenated past binaries) and `.tb_mvh` (header: 8192 bytes of release numbers, offsets, sizes, checksums, flags, client minimums). `parse_elf` requires `.tb_mvh` to be the last section and `.tb_mvb` the second-to-last. They're `noload` so the loader doesn't map up to hundreds of MiB of old binaries into the running process. They're only read as file bytes, when the replica copies the binary into its memfd or extracts a past release.

**Q2.** Ops between the checkpoint op and the trigger are executed **before** the checkpoint is written, but they stay in the WAL and are **replayed** after a restart from that checkpoint. If the checkpoint says `release = v2`, the restarted replica replays the last bar with `v2`, though `v1` already executed it. If one op there is a real transfer batch and `v2` computes a different result (new validation, bug fix), the replayed reply won't match the reply stored in the client sessions, and an assertion trips. Without that assertion, state would silently diverge. `upgrade` ops are no-ops, so `v1` and `v2` agree on them by construction. Hence `release_for_next_checkpoint` returns the new release only when `found_upgrade == lsm_compaction_ops`.

**Q3.** The primary stops accepting client requests and chains upgrade ops until the next trigger: up to ~960 ops to reach the next checkpoint, plus the 32-op last bar, ≈ 992 ops. At 3 ms each, that's ≈ 3 s, plus the restart (exec, open, short replay). That's consistent with the docs' "on the order of 5 seconds". This is an estimate; actual per-op latency depends on network and disk. Checkpointing early would shorten the window but break the fixed checkpoint schedule. Checkpoint ops are a deterministic function of the op number (`checkpoint_after`), and WAL slot reuse, compaction pacing (beats/bars) and the "previous checkpoint quorum before overwriting the WAL" invariant all depend on that. An ad-hoc checkpoint would have to be agreed through consensus and would complicate every one of those invariants.

**Q4.** The two upgraded replicas checkpoint with `release = v2`, exec into `v2`, and continue committing (2 of 3 is a replication and view-change quorum). The third replica, still on `v1` only, eventually reaches or state-syncs to a checkpoint whose `release` is `v2`. `release_transition` then calls `release_execute(v2)`, which isn't bundled, so the replica panics with "release not available". It stays down until the operator swaps its binary. The cluster has silently lost its fault tolerance: one more failure halts it. The vsr.zig comment says so: not upgrading all replicas together "would be a mistake (leading to replicas lagging and needing to state sync)". A compromised cluster should use a hotfix build with the same release number instead.

**Q5.** Open-ended. A strong answer notes that physical determinism means more than the state machine: LSM compaction (which tables merge, output bytes), grid block allocation (free set), checkpoint/superblock encoding, manifest log and client-session encoding all affect bytes on disk and checksums compared across replicas. All of them would need versioning. Reasonable picks:
- **Whole-binary** for robustness, with the cost being Linux-specific exec plumbing and a release pipeline.
- **In-binary dispatch** limited to the state machine while the storage format is frozen, switching to whole-binary once storage evolves.

Either way, the first release must store a `release` field in the checkpoint/superblock (and ideally a `release_format` recording who formatted the file). It must also reserve an `upgrade` operation number in the control-plane range, so a later release can run the checkpoint switch protocol against data written by release 1.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
