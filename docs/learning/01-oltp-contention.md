# Concept 1: The OLTP contention problem

**Module:** M0 The problem · **Status:** 🟡 in progress

**Sources:**
- `../tigerbeetle/docs/concepts/oltp.md`
- `../tigerbeetle/docs/ARCHITECTURE.md`, sections "Problem Statement", "Single Thread" and "Non-Interactive Transactions"
- `../tigerbeetle/docs/concepts/performance.md`, section "Single Threaded By Design"

## 1. The problem

Online transaction processing (OLTP) means recording business events as they happen: payments, metering, billing. TigerBeetle's problem statement describes workloads that:

- consist **mostly of writes**,
- have **contention**, so they parallelize and shard poorly (Amdahl's law),
- need very high throughput with moderately low latency,
- need strong consistency and very high durability.

Every business transaction touches **at least two accounts**, often more for fees, taxes and revenue splits.

Account usage is **Pareto-distributed**: a few hot accounts, such as the merchant's revenue account or the fee account, appear in a huge share of transfers.

## 2. The obvious solution, and why it fails

In a general-purpose database like Postgres, a transfer is an **interactive transaction**:

```sql
BEGIN;
SELECT ... FROM accounts WHERE id IN ($a, $b) FOR UPDATE;  -- lock both accounts
-- ⟵ network round trip: the app computes the new balances ⟶
UPDATE accounts SET balance = ... WHERE id = $a;
UPDATE accounts SET balance = ... WHERE id = $b;
COMMIT;                                                    -- locks released
```

The lock on a hot account is held **across network round trips**. So the hot account's throughput is capped at:

```
max TPS on a hot account ≈ 1 / lock_hold_time
```

That holds however many cores or machines you have. Most of the hold time is the network, not useful work.

### Why more hardware doesn't help

- A hot account is a **serial section** of the workload. **Amdahl's law:** if a fraction `s` of the work is serial, the speedup can never exceed `1/s`.
- **Sharding makes it worse.** Hot accounts can't be partitioned cleanly, and a transfer between shards needs coordination such as two-phase commit. That adds *more* round trips while the locks are held.
- **Batch billing with MapReduce** sidesteps contention, but you lose real-time balances and spending limits.

## 3. What TigerBeetle does

Concepts 3, 6 and 7 cover each of these in depth.

- **Move the logic into the database.** Debit/credit rules run inside TigerBeetle, so no lock is ever held over the network.
- **Use one core and no locks at all.** Execution is sequential and strictly serializable by construction.
- **Batch everything.** A request carries up to ~8,190 transfers, so consensus and replication cost is paid **once per batch**.
- **Replicas add reliability, not throughput.** This is a deliberate trade: "Adding more nodes can therefore increase reliability, but not throughput."
- **Design target:** 1 million transactions per second on a single core.

## 4. What it means for ashi

- To compete on this workload, ashi has to match the **interface** (debit/credit, batches, no interactive transactions) first. The language comes second.
- Rust doesn't change the physics. A lock-free single-writer core is just as natural in Rust.
- The open question for ashi's differentiation is whether any workload segment is *not* dominated by hot accounts and could benefit from multiple cores. That's drill question 4.

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt** the problem framing: write-heavy, contended and hard to shard, with batches and no interactive transactions. Explore **differ** only for partitioned multi-tenant ledgers, or for pipelining work off the single commit core.

## 5. Drill questions

### Q1. Rough calculation

Postgres, with the app server 0.5 ms round trip away, running the interactive transaction above.

Every transfer also credits a single shared `fees` account. Holding that lock takes about 2 round trips plus ~0.1 ms of database work.

- What's the rough maximum transfers per second for the **whole system**?
- Does it change on a 64-core machine?

> _Your answer:_

### Q2. Mechanism

Explain in your own words why sharding by account ID makes this workload *slower*. Name what gets added.

> _Your answer:_

### Q3. Counterintuitive claim

TigerBeetle's docs say processing *gets faster* under hot-account contention. Why would contention help a single-threaded engine when it hurts Postgres?

Hint: think about the hardware, not locks.

> _Your answer:_

### Q4. ashi stance (open-ended)

Is there any workload where ashi could legitimately beat TigerBeetle by using multiple cores? What would have to be true about that workload?

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1. Rough calculation**

- Lock hold time on `fees` ≈ 2 × 0.5 ms + 0.1 ms = **1.1 ms**.
- Every transfer must take that lock, so the whole system serializes on it: about **1 / 1.1 ms ≈ 900 transfers/s**.
- 64 cores change nothing. The bottleneck is a serial section, not CPU. It can get *worse*: more concurrent transactions queue on the lock, and lock-manager and deadlock-detection overhead grows.
- By Little's law, once arrivals exceed ~900/s the queue grows without bound and latency explodes.

**Q2. Mechanism**

- A transfer between accounts on two shards needs **distributed coordination**, usually two-phase commit: prepare on both shards, then commit on both.
- That adds inter-shard round trips *while locks are held*. Lock hold time goes up, so the hot account's ceiling (1 / hold time) goes down.
- The hot account still lives on exactly one shard, so that shard is the bottleneck either way.
- Two-phase commit also brings coordinator-failure and blocking problems.

**Q3. Counterintuitive claim**

- For a single thread with no locks, contention means **locality**.
- Hot accounts stay in the CPU caches (L1/L2) and in TigerBeetle's in-memory object caches and upper LSM levels.
- Many transfers in a batch hit the same few account records, so lookups are cache hits instead of memory or disk reads.
- There are no locks to fight over. "Contention" just becomes repeated work on hot data, which hardware is very good at.

**Q4. ashi stance (open-ended)**

Multiple cores can win only where the work is genuinely independent, or where it is off the critical commit path:

- **Partitioned ledgers.** Think multi-tenant SaaS, where each tenant's ledger never transfers to another. Each partition gets its own single-writer core (shared-nothing, thread-per-core). This requires cross-partition transfers to be rare or nonexistent, hot accounts to stay inside one partition, and load to be roughly balanced across partitions.
- **Pipelining around one commit core.** Checksumming, network parsing, compaction and read-only queries on snapshots can run on other cores, while one core stays the sole writer of balances. TigerBeetle runs everything on one thread, so this is a credible place for ashi to differ. The costs: coordination overhead, and a harder deterministic-simulation story (see Concept 29).

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
