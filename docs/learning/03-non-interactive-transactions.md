# Concept 3: Transactions run inside the database, not over network round trips

**Module:** M0 The problem · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Problem Statement", "Non-Interactive Transactions" and "Synchronous Execution"
- `../tigerbeetle/docs/concepts/performance.md`, section "It's All About The Interface"
- `../tigerbeetle/docs/concepts/debit-credit.md`, sections "SQL vs Debit/Credit" and "TigerBeetle Enforces Debit/Credit in the Database"
- `../tigerbeetle/docs/coding/requests.md`, section "Guarantees"
- `../tigerbeetle/docs/coding/two-phase-transfers.md`, `linked-events.md`
- `../tigerbeetle/docs/coding/recipes/balance-conditional-transfers.md`, `balance-bounds.md`, `rate-limiting.md`, `multi-debit-credit-transfers.md`
- `../tigerbeetle/src/state_machine.zig` (`commit`, `execute_create`, `create_transfer`)
- `../tigerbeetle/src/tigerbeetle.zig` (`Operation`)
- `../tigerbeetle/src/cdc/runner.zig` (change-data-capture consumer of `get_change_events`)

## 1. The problem

A transfer is a tiny program: read two balances, check a rule, write two balances. The question is **where that program runs**.

In a general-purpose database (Postgres, MySQL), the program runs in the *application*. The database holds the data and the locks. For each transfer, data has to travel to the code and back over the network.

For ledgers that is the wrong way round:

- Most transactions **write**, and many touch the same hot accounts (Concept 1).
- Whatever protects a hot account (a lock, or an optimistic version check) is held, or exposed, for the whole time the program runs.
- If the program's runtime includes network round trips, the hot account's throughput is set by **network latency**, not by CPU speed.

## 2. The obvious solution, and why it fails

### Interactive transactions

`ARCHITECTURE.md` ("Non-Interactive Transactions") lists the usual five steps:

```
1. Open a transaction                       ──┐
2. Fetch balances        (acquires lock)      │ lock held
3. Compute update        (in the app)         │ across
4. Send updates                               │ round trips
5. Commit                (releases lock)    ──┘
```

> "Crucially, step 2 acquires a lock on the balances which is not released until step 5. A lock is held over a network round-trip."

A worked number. The app is 1 ms round trip from the database, the transaction makes 3 round trips while the lock is held, and the database does 50 µs of real work:

```
lock_hold ≈ 3 × 1 ms + 0.05 ms = 3.05 ms
max TPS on one hot account ≈ 1 / 3.05 ms ≈ 330
useful work share ≈ 0.05 / 3.05 ≈ 1.6%
```

More than 98% of the lock time is spent waiting on the network.

### "Just use optimistic concurrency"

Don't lock. Read a version number, and at commit time abort if someone else changed the row. With low contention this is great. With a hot account nearly every concurrent transaction conflicts, so most attempts abort and retry. The retries add load exactly when the system is busiest. The time window is still the round trips, so the window for conflicts is just as wide.

### "Just use stored procedures"

Closer: the code moves to the data. But the general-purpose engine still does per-row locking, MVCC (multi-version concurrency control) bookkeeping, and per-statement work sized for arbitrary SQL. And the "on the order of 10 SQL queries per transaction" (`debit-credit.md`) are still 10 queries, just executed closer to the data. You get the location right, but the interface is still too general.

## 3. What TigerBeetle does

**Move the code to the data, and make the code fixed.** A client never reads, decides and writes. It sends *intent*: "debit A, credit B, amount X, under these flags". The database decides and replies with a result code.

> "With TigerBeetle, all the logic lives inside the database, obviating the need for locking." (`performance.md`)

### 3.1 The request is the transaction

```
 client/gateway                          TigerBeetle primary
 ──────────────                          ───────────────────
 build batch of N Transfers  ──request──▶ order request (consensus, Concept 21)
 (no reads needed)                        prefetch: load accounts/transfers (IO, parallel)
                                          execute:  for each event, create_transfer()
                                                    (pure CPU, sequential, no locks)
                             ◀──reply───  one result per event
```

- **One round trip per batch, and no lock across it.** The only "lock" is the single thread doing the execute step (Concept 6). Its hold time is the CPU time for the batch, not a network time.
- **Execution never waits on IO.** `ARCHITECTURE.md` ("Synchronous Execution") says the `commit` function "is synchronous… doesn't do any reading from storage". All IO happens earlier, in prefetch (Concept 10).
- **Fixed operations.** The set of programs is closed: the `Operation` enum (`src/tigerbeetle.zig:679`), with `create_accounts`, `create_transfers`, lookups and queries. `commit` (`src/state_machine.zig:2538`) dispatches on it. For creates, `execute_create` (`state_machine.zig:2976`) is a plain `for` loop calling `create_account` or `create_transfer` once per event.

The guarantees of a request (`requests.md`, "Guarantees") are what an interactive transaction would have given you, minus the round trips:

- Events execute in sequence; each sees the effects of the previous ones.
- Events in a request don't interleave with events from other requests.
- The whole request commits, or none of it does. Individual events still succeed or fail **independently**, unless they are linked.

### 3.2 What you give up

You can no longer run arbitrary logic between the read and the write. Things a fixed-function engine cannot express in one event:

- "Charge a fee equal to 1.5% of the *current* balance." The app can't read the balance inside the transaction.
- "Transfer only if account C (not A or B) has at least 1,000."
- "Ask the fraud service, then decide." No external calls are allowed during execution. They would also break determinism across replicas (Concept 20).
- Any predicate over arbitrary fields, joins, or loops of unbounded length.

The obvious workaround, `lookup_accounts` then `create_transfers`, is explicitly unsafe: "these requests are not atomic and the account's balance may change between the lookup and the transfer" (`recipes/balance-conditional-transfers.md`).

### 3.3 How TigerBeetle compensates: a small, composable vocabulary

Instead of a general language, TigerBeetle offers a handful of primitives that compose inside one request. Each is a flag or a field on the 128-byte `Transfer` (Concept 2):

| Need (that would be app logic elsewhere) | TigerBeetle primitive |
|---|---|
| All-or-nothing multi-leg transaction | `flags.linked` chains (`linked-events.md`) |
| "Never go negative" | `debits_must_not_exceed_credits` / `credits_must_not_exceed_debits` on the account |
| "Only if balance ≥ threshold" | pending + void inside a linked chain (`recipes/balance-conditional-transfers.md`) |
| "Keep balance between bounds" | 5-transfer linked chain via a control account (`recipes/balance-bounds.md`) |
| "Move as much as possible, up to X" | `balancing_debit` / `balancing_credit` |
| "Reserve now, decide later" | `pending` + `post_pending_transfer` / `void_pending_transfer` |
| "Auto-cancel if nobody decides" | `timeout` on a pending transfer |
| Rate limiting | pending transfers with timeouts, left to expire (`recipes/rate-limiting.md`) |
| Freeze an account | `closing_debit` / `closing_credit` (pending, so reversible) |
| Correlate with business entities | `user_data_128/64/32`, `code` |
| React downstream (emails, analytics) | `get_change_events` operation (`tigerbeetle.zig:695`), consumed by the CDC runner (`src/cdc/runner.zig`) |

**Worked example: the balance-conditional transfer.** "Move 50 from Source to Destination only if Source holds at least 1,000." Source has a credit balance and `debits_must_not_exceed_credits`:

```
T1  Source  → Control      1000   pending, linked        # fails if Source < 1000
T2  (void T1)                     void_pending, linked   # give the 1000 straight back
T3  Source  → Destination    50                          # the real transfer
```

- If T1 fails the limit check, the whole chain fails and T3 never happens.
- If T1 passes, T2 undoes it, and T3 runs.

The "read and branch" happens inside the database, atomically, using only flags.

### 3.4 Two-phase transfers turn a held lock into data

When an app *must* do slow external work (fraud check, card network), two-phase transfers replace the held lock:

```
interactive:   lock(A) ── call fraud service (200 ms) ── update A ── unlock(A)
                 └──────── every other transfer on A waits ────────┘

two-phase:     T_pending(A→B, 100, timeout=30s)    # one fast event; 100 now in debits_pending
               ... call fraud service, no lock held ...
               T_post(pending_id) or T_void(pending_id)
```

- The reservation is **data** (`debits_pending`), not a mutex. Other transfers on `A` keep flowing; they just see 100 less available balance.
- It is durable and replicated like any other transfer.
- It has a **timeout**, so a crashed app can't leave money reserved forever.
- It resolves **at most once** (`pending_transfer_already_posted` / `_voided` / `_expired`).

This is the key reframing: **contention moves from "who holds the lock" to "how much is reserved"**, and the second can be checked in O(1) by the single-threaded engine.

### 3.5 Why the fixed interface is also faster on the CPU

- Every event in a `create_transfers` batch runs the same function on the same 128-byte type. The "which kind of event?" branch is hoisted out of the loop. `ARCHITECTURE.md` ("Mechanical Sympathy") calls this lifting the branching up. Concepts 8 and 9 cover it.
- There is nothing to parse or plan: no SQL, no query optimizer, no per-statement locking.

## 4. What it means for ashi

**Interface first.** ashi's request type should be "a batch of fixed-size intents" (`&[Transfer]`), not a transaction handle the client drives step by step. Once a client can say `begin; read; …; write; commit`, the round-trip lock is back.

**Rust features that help:**
- The state machine can be a plain `fn create_transfer(&mut self, ts: u64, t: &Transfer) -> CreateTransferStatus`. `&mut self` on a single owner gives single-writer semantics checked at compile time, with no `Mutex` or `RwLock`.
- Keep the execute path **synchronous**: no `async fn`, no `.await`. An `.await` inside commit is a suspension point where something else could interleave. That is the interactive pattern again, inside one process.
- `enum Operation` plus an exhaustive `match` keeps the set of programs closed and checked.

**Pitfalls specific to Rust:**
- **The async ecosystem pulls the other way.** The idiomatic tokio service spawns a task per request and shares state as `Arc<Mutex<Ledger>>`. That is the lock-per-round-trip design, just in-process. Prefer one owner thread fed by a queue of batches, which is the actor/single-writer pattern.
- **No callbacks in commit.** No `Box<dyn Fn(&Account) -> bool>` hooks or user closures in the commit path. They make execution time unbounded (Concept 11) and can break determinism.
- **No hidden IO.** Execution must not touch storage. Make that structural: give the execute function only an in-memory view (for example a prefetched `&mut Cache`), never a handle that can do IO.

**If ashi wants programmability**, this is the one real design fork versus TigerBeetle. Options:
- More fixed primitives: new flags or new `Operation` variants. Cheap, deterministic, and testable by the simulator (Concept 29).
- A sandboxed, metered user program (for example WASM via `wasmtime` with fuel limits, or a tiny custom bytecode). The costs: a large dependency (TigerBeetle has zero, Concept 13), determinism must be proven (no floats, no clock, no randomness), a hard upper bound on steps per event is needed to keep static limits, and every replica must run the identical program version (Concept 32).

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt** non-interactive, fixed-function execution as the core. Treat any programmability as a later, strictly bounded extension. Never offer an interactive transaction handle.

## 5. Drill questions

### Q1. Recall

List the five steps of an interactive transaction from `ARCHITECTURE.md`. Which step acquires the lock and which releases it? What does TigerBeetle's client send instead?

> _Your answer:_

### Q2. Mechanism

Explain how a pending transfer with a timeout replaces "hold a lock on account A while calling a fraud service". What exactly plays the role of the lock, and why doesn't it block other transfers on A?

> _Your answer:_

### Q3. Back-of-envelope

A team runs interactive Postgres transactions from an app in another region: 30 ms round trip, 2 round trips while the hot `fees` account is locked, 0.1 ms of database work.

1. What is the maximum transfers per second through `fees`?
2. TigerBeetle sits at the same 30 ms distance. One request carries up to 8,189 transfers (`requests.md`), and every one credits `fees`. With only one request in flight at a time, what is the throughput ceiling set by latency alone?
3. What does that tell you about where TigerBeetle's real bottleneck is?

> _Your answer:_

### Q4. What breaks if…

…TigerBeetle's `create_transfer` were allowed to make a synchronous HTTP call to a fraud service during execution? Name at least three things that break, linking each to a concept number.

> _Your answer:_

### Q5. ashi design (open-ended)

Should ashi offer user-defined transfer logic (for example a WASM hook per `code`)? If yes, what constraints must it satisfy to keep the single-threaded, deterministic, statically bounded core? If no, what new built-in primitive would you add first?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- The five steps: (1) open a transaction, (2) fetch balances, (3) compute the update in the app, (4) send updates, (5) commit.
- Step 2 acquires the lock; step 5 releases it. So the lock spans at least one network round trip.
- TigerBeetle's client instead sends a batch of `Transfer` intents: debit/credit ids, amount, flags. The database runs the fixed `create_transfer` logic and returns one result per event, with no reads needed first.

**Q2.**
- The pending transfer moves the amount into `debits_pending` on A (and `credits_pending` on B). The balance-limit check counts `debits_pending` against A, so the reserved money can't be spent twice. That reservation *is* the lock, but it is a number in a record, not a held mutex.
- Other transfers on A keep executing immediately. They see a smaller available balance, and they fail only if *they* would exceed it.
- The fraud call happens outside the database, with nothing held. Afterwards the app posts or voids by `pending_id`. If the app dies, `timeout` expires the reservation deterministically.

**Q3.**
1. Hold ≈ `2 × 30 + 0.1 = 60.1 ms`, so about **16.6 transfers/s** through `fees`, whatever the hardware.
2. `8,189 / 30 ms ≈ 273,000 transfers/s` from latency alone, with a single request in flight. Latency no longer caps per-account throughput, because one round trip is amortised over the whole batch and nothing is locked during it.
3. The ceiling moves from the network to CPU work per transfer, plus amortised replication and IO per batch. That is the "speed of light" framing of Concept 5. Hot-account contention is now just sequential CPU work, which Concept 1 noted can even get *faster* with hot accounts because they stay in cache.

**Q4.**
- **Determinism (Concept 20).** Replicas re-execute the same prepare. The HTTP answer can differ between replicas or between replay and the original run, so the state diverges.
- **Single-thread throughput (Concepts 5 and 6).** Every transfer on every account waits behind a 100 ms call. That is the round-trip lock again, now global.
- **Static bounds (Concept 11).** The call's latency and memory are unbounded, and the commit loop's worst-case time is unknown.
- **Synchronous commit (Concept 10).** Execution must not do IO; this adds IO in the hottest loop.
- **Simulation testing (Concept 29).** The VOPR can't deterministically simulate an external service it doesn't control.

**Q5.** Open-ended. A strong answer covers:
- **Constraints for any hook:** pure function of (event, prefetched accounts); no IO, clock, randomness or floats; a hard step/fuel limit per event and per batch; static memory; versioned and replicated with the binary or log (Concept 32); fuzzed and simulated.
- **Prefetch must still be possible.** The hook must declare which accounts it may read *before* execution, or prefetch can't load them (Concept 10).
- **Honest cost:** a big dependency, a larger attack surface, and harder auditing.
- **The alternative:** add primitives one at a time. Candidates: a native "balance ≥ threshold" condition flag, or percentage-based fees with a fixed rounding rule. Each is a few lines in `create_transfer` and stays inside the 128-byte record.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
