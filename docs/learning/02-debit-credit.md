# Concept 2: Debit/credit as the schema, and its invariants

**Module:** M0 The problem · **Status:** ⬜ not started

**Sources:**
- `../tigerbeetle/docs/concepts/debit-credit.md`
- `../tigerbeetle/docs/reference/account.md`, `../tigerbeetle/docs/reference/transfer.md`
- `../tigerbeetle/docs/reference/requests/create_transfers.md`, sections "status" and "flags_are_mutually_exclusive"
- `../tigerbeetle/docs/coding/data-modeling.md`, `two-phase-transfers.md`, `linked-events.md`
- `../tigerbeetle/docs/coding/recipes/close-account.md`, `balance-conditional-transfers.md`, `correcting-transfers.md`
- `../tigerbeetle/src/tigerbeetle.zig` (`Account`, `Transfer`, flags, status enums)
- `../tigerbeetle/src/state_machine.zig` (`execute_create`, `create_account`, `create_transfer`, `post_or_void_pending_transfer`, `ExpirePendingTransfersWorkerType`)
- `../tigerbeetle/src/state_machine_tests.zig`, test "linked accounts"

## 1. The problem

A ledger has one job: never lose track of money. Every business event (a payment, a fee, a refund) must be recorded so that:

- money never appears from nowhere and never vanishes,
- balances never break business rules (a customer can't spend money they don't have),
- history is never rewritten, so an auditor can replay it.

These are **invariants**: statements that must be true after every single operation. A ledger is only as good as the weakest place where an invariant is checked.

Concept 1 showed that ledger workloads are write-heavy and contended. This lesson is about *what* gets written: the schema, and the rules that schema enforces.

## 2. The obvious solution, and why it fails

The obvious design is a SQL table per business concept (`payments`, `refunds`, `wallets`, `fees`) with a signed `balance` column, and rules enforced in application code:

```sql
-- app code, "check then act"
SELECT balance FROM wallets WHERE id = $a;          -- app sees 100
-- app decides: 100 >= 80, OK
UPDATE wallets SET balance = balance - 80 WHERE id = $a;
UPDATE wallets SET balance = balance + 80 WHERE id = $b;
```

What goes wrong:

- **Check-then-act races.** Two requests both read `100`, both decide `80` is fine, and the wallet ends at `-60`. The fix is row locks, which brings back Concept 1's contention.
- **Rules live in N places.** Every service that writes to `wallets` must re-implement "no negative balance". One forgotten code path breaks the invariant for everyone.
- **One-sided writes.** Nothing forces the second `UPDATE`. A crash or bug between the two statements creates or destroys money.
- **Destructive history.** `UPDATE` and `DELETE` overwrite the past. A bad migration can silently lose a year of records.
- **Schema sprawl.** Each new product adds tables and columns. TigerBeetle's docs put a single business transaction at "on the order of 10 SQL queries per transaction" (`debit-credit.md`, "SQL vs Debit/Credit").

TigerBeetle's own recipe for balance checks says it plainly: checking a balance with `lookup_accounts` and then transferring "would be unsafe… because these requests are not atomic" (`balance-conditional-transfers.md`).

## 3. What TigerBeetle does

TigerBeetle replaces the ad-hoc schema with **double-entry bookkeeping** (debit/credit), a model accountants have used for centuries. It has two entities, `Account` and `Transfer`, and one core invariant: every debit has an equal and opposite credit. All other rules are enforced *inside* the database, at commit time.

### 3.1 The `Account` record: 128 bytes

`src/tigerbeetle.zig:10`:

```zig
pub const Account = extern struct {
    id: u128,
    debits_pending: u128,
    debits_posted: u128,
    credits_pending: u128,
    credits_posted: u128,
    user_data_128: u128,
    user_data_64: u64,
    user_data_32: u32,
    reserved: u32,
    ledger: u32,
    code: u16,
    flags: AccountFlags,
    timestamp: u64,

    comptime {
        assert(stdx.no_padding(Account));
        assert(@sizeOf(Account) == 128);
        assert(@alignOf(Account) == 16);
    }
```

```
offset  size  field              who sets it
0       16    id                 client (not 0, not 2^128-1)
16      16    debits_pending     database (must be 0 on create)
32      16    debits_posted      database
48      16    credits_pending    database
64      16    credits_posted     database
80      16    user_data_128      client (opaque)
96       8    user_data_64       client (opaque)
104      4    user_data_32       client (opaque)
108      4    reserved           must be 0
112      4    ledger             client (not 0)
116      2    code               client (not 0)
118      2    flags              client
120      8    timestamp          database (0 unless imported)
        ---
        128 bytes, no padding, 16-byte aligned
```

- `extern struct` means C layout: fields in declaration order. The `comptime` block fails the *build* if the size, alignment or padding ever drifts.
- There is **no signed balance**. TigerBeetle stores four monotonically growing counters. The application computes `credits - debits` or `debits - credits`, depending on account type (`data-modeling.md`, "Debits vs Credits").
- Amounts are `u128` integers. Fractions use an **asset scale**: the smallest useful unit maps to `1` (USD cents: scale 2, so `123.45` is `12345`).

### 3.2 The `Transfer` record: also 128 bytes

`src/tigerbeetle.zig:85`:

```
offset  size  field              notes
0       16    id                 client-generated idempotency key (Concept 4)
16      16    debit_account_id
32      16    credit_account_id  must differ from debit_account_id
48      16    amount             u128; meaning depends on flags
64      16    pending_id         only for post/void: id of the pending transfer
80      16    user_data_128
96       8    user_data_64
104      4    user_data_32
108      4    timeout            seconds; only for pending transfers
112      4    ledger
116      2    code               the "why" of the transfer
118      2    flags              TransferFlags
120      8    timestamp          assigned by the cluster
```

Notice that bytes 80–127 have the same layout in both records (`user_data_*`, then a `u32`, `ledger`, `code`, `flags`, `timestamp`). A transfer debits exactly **one** account and credits exactly **one** account on the **same ledger**. Multi-leg transactions are composed with linked events (3.6).

### 3.3 Ledgers and codes

- **`ledger` (u32)** partitions accounts into groups that can transact with each other, typically one per currency or asset. `create_transfer` rejects cross-ledger moves (`src/state_machine.zig:3769`):

  ```zig
  if (dr_account.ledger != cr_account.ledger) return .accounts_must_have_the_same_ledger;
  if (t.ledger != dr_account.ledger) {
      return .transfer_must_have_the_same_ledger_as_accounts;
  }
  ```

  Currency exchange is two linked transfers through a liquidity provider's accounts on each ledger (`recipes/currency-exchange.md`).
- **`code` (u16)** is a user-defined enum: on an account it is the account type ("cash", "revenue"); on a transfer it is the reason ("refund", "fee"). Both `ledger` and `code` must be nonzero.
- Names such as "USD" or "refund" live in the application or a general-purpose database. TigerBeetle stores only integers (`system-architecture.md`).

### 3.4 Account flags and balance limits

`AccountFlags` is a `packed struct(u16)` (`src/tigerbeetle.zig:45`): one bit per flag, with the unused bits as padding that must be zero.

```
bit 0  linked
bit 1  debits_must_not_exceed_credits
bit 2  credits_must_not_exceed_debits
bit 3  history          (keep balance history for get_account_balances)
bit 4  imported         (client supplies a historical timestamp)
bit 5  closed
bits 6-15 padding (must be 0, else reserved_flag)
```

> ⚠️ **Docs vs code (minor):** for `imported` events, the field sections of `account.md` and `transfer.md` say the timestamp must be "greater than `0` and not be larger than `2^63`". The code accepts `1 ..= maxInt(u63)`, which is `2^63 - 1` (`src/lsm/timestamp_range.zig:10`), so `2^63` itself is rejected. The table in `transfer.md` ("`> 0` and `< 2^63`") matches the code.

The two balance-limit flags are the database-enforced "no overdraft" rule (`src/tigerbeetle.zig:34`):

```zig
pub fn debits_exceed_credits(self: *const Account, amount: u128) bool {
    return (self.flags.debits_must_not_exceed_credits and
        self.debits_pending + self.debits_posted + amount > self.credits_posted);
}
```

Two details matter:

- **Pending debits count against you; pending credits do not count for you.** The check uses `debits_pending` but only `credits_posted`. Reserved money is unavailable. Incoming money that has not posted yet is not spendable.
- A customer wallet is a liability for the operator, so it has a credit balance. It uses `debits_must_not_exceed_credits` to stay non-negative. The two limit flags together are rejected with `flags_are_mutually_exclusive` in `create_account`.

Overflow is also an invariant. `create_transfer` rejects any transfer that would overflow `debits_pending`, `debits_posted`, or the pending+posted sum, using `sum_overflows` (`src/state_machine.zig:5377`), which wraps `std.math.add` and treats an error as overflow. Overflow is never allowed to wrap silently.

### 3.5 Two-phase transfers: pending, post, void, expire

A **two-phase transfer** reserves funds first and decides later (card authorisation, then capture).

```
phase 1: pending          phase 2: exactly one of
  flags.pending             post   (flags.post_pending_transfer, pending_id = P.id)
  amount -> *_pending       void   (flags.void_pending_transfer, pending_id = P.id)
                            expire (cluster-driven, after timeout seconds)
```

`create_transfer` puts a pending amount into the `_pending` counters and records a side entry (`src/state_machine.zig`, after the "must succeed" comment at line 3889):

```zig
if (t.flags.pending) {
    dr_account_new.debits_pending += amount_actual;
    cr_account_new.credits_pending += amount_actual;

    self.forest.grooves.transfers_pending.insert(&.{
        .timestamp = timestamp_actual,
        .status = .pending,
    });
}
```

`post_or_void_pending_transfer` (`src/state_machine.zig:4036`) resolves it:

- It always releases the **full** pending amount: `debits_pending -= p.amount`.
- **Post** then adds `amount_actual` to `_posted`. `amount_actual` is `p.amount` if the request's amount is `AMOUNT_MAX` (`2^128 - 1`), otherwise the requested amount, which must be ≤ `p.amount` (`state_machine.zig:4103`, `4108`). Posting less than reserved returns the rest.
- **Void** must match the pending amount exactly (or pass `0` to inherit it).
- The pending's status moves from `pending` to `posted`/`voided`/`expired` in a separate 16-byte `TransferPending { timestamp, status }` record (`src/state_machine.zig:93`). **The original pending `Transfer` is never modified.**
- A second resolution fails with `pending_transfer_already_posted`, `_already_voided` or `_expired`. A pending transfer resolves at most once.

**Timeouts.** `timeout` is a `u32` number of seconds, relative to the transfer's cluster timestamp, not an absolute time (more robust to client clock skew; see Concept 26). Expiry is deterministic:

- It runs as an internal `pulse` operation. `ExpirePendingTransfersWorkerType` (`src/state_machine.zig:5039`) keeps `timestamp_next`, the earliest known expiry. `pulse_needed` fires when `timestamp_next <= timestamp`.
- The worker (`execute`, `state_machine.zig:5243`) gives back `debits_pending`/`credits_pending` and marks the pending `expired`.
- `create_transfer` also rejects a timeout whose expiry `timestamp + timeout * 10^9` would not fit in a `u63` (`overflows_timeout`). The top bit of the `u64` timestamp is reserved as a tombstone flag (`state_machine.zig:3873`).

Because the pending amount was already checked against limits when reserved, **post and void can never break a balance limit**. That is why post/void are mutually exclusive with the balancing flags (`two-phase-transfers.md`, "Interaction with Account Invariants").

### 3.6 Linked events: atomic chains

Events in one request succeed or fail independently, unless they are chained with `flags.linked`. The chain ends at the first event *without* the flag. The logic is in `execute_create` (`src/state_machine.zig:2976`):

```
batch:  [A]  [B linked] [C linked] [D]  [E]
              \______ chain ______/
                                         A and E stand alone.
```

1. **Open.** On the first linked event, the state machine opens a *scope* on every affected tree (`scope_open`, `state_machine.zig:2934`): accounts, transfers, transfers_pending and account_events.
2. **Fail.** If any event in the chain returns anything other than `created`, the scope is discarded (`scope_close(.discard)`), every earlier event in the chain is rewritten to `linked_event_failed`, and the rest of the chain is skipped (`state_machine.zig:3091–3122`).
3. **Commit.** If the chain ends cleanly, `scope_close(.persist)` keeps it.
4. **Open-ended chain.** If the *last* event of the batch has `linked` set, it gets `linked_event_chain_open` (`state_machine.zig:3013`).

Events inside a chain see each other's effects. The `balance-conditional-transfers` recipe relies on this: event 2 voids the pending transfer created by event 1 in the same chain.

> ⚠️ **Docs vs code (subtle):** `linked-events.md` says the first event to fail gets "a unique error result". The code treats **`exists` as a failure** too (`result_status != .created`). The test "linked accounts" (`state_machine_tests.zig:1526`) labels this "Fail with .exists". So re-submitting an already-committed chain returns `exists` for the first event and `linked_event_failed` for the rest. Concept 4 picks this up.

### 3.7 Balancing transfers

`flags.balancing_debit` / `balancing_credit` turn `amount` into an **upper bound**. The database moves as much as the account allows (`state_machine.zig:3824`):

```zig
if (t.flags.balancing_debit) {
    const dr_balance = dr_account.debits_posted + dr_account.debits_pending;
    amount = @min(amount, dr_account.credits_posted -| dr_balance);
}
```

- `-|` is Zig's saturating subtraction: it stops at 0 instead of wrapping.
- The stored transfer records the **actual** amount, which may be 0. A zero-amount transfer is still `created`.
- Use `AMOUNT_MAX` for "sweep everything", for example to zero an account without first querying its balance (`recipes/close-account.md`).

### 3.8 Closing accounts

`Account.flags.closed` makes an account reject further transfers. It can be set at creation, or by a **pending** transfer with `closing_debit`/`closing_credit`:

- A non-pending closing transfer fails with `closing_transfer_must_be_pending` (`state_machine.zig:3736`). Requiring a pending transfer makes the close reversible, and the reversal has to reference *that specific* close.
- Closed accounts reject new transfers (`debit_account_already_closed`). The one exception is voiding a pending transfer (`state_machine.zig:4177`):

  ```zig
  // The only movement allowed in a closed account is voiding a pending transfer.
  if (dr_account.flags.closed and !t.flags.void_pending_transfer) {
      return .debit_account_already_closed;
  }
  ```

- Voiding the closing transfer reopens the account (`state_machine.zig:4249`). So does its **expiry** (`state_machine.zig:5294`).
- A closing transfer can never be **posted**. Its own account is closed, so the post fails the check above. `post_or_void_pending_transfer` asserts this (`assert(!p.flags.closing_debit)`).

> ⚠️ **Docs vs code:** `recipes/close-account.md` says existing pending transfers on a closed account "can still time out but can't be posted or voided." The code allows **voiding** (line 4177 above), and `reference/account.md` agrees with the code. Also, `account.md` says only voiding reopens an account. `transfer.md` and the expiry code (`state_machine.zig:5294`) show that expiry reopens it too.

### 3.9 Immutability

- **Transfers are insert-only.** No operation modifies or deletes one. Even resolving a pending transfer writes a *new* transfer plus a status row in `transfers_pending`.
- **Accounts are immutable except** for the four balance counters and the `closed` bit, which only transfers can change. The `AccountsGroove` marks the four counters, `flags` and `reserved` as `.ignored`, meaning they get no secondary index, which suits fields that change (`state_machine.zig:410`).
- **Mistakes are fixed by adding** correcting transfers, never by editing (`recipes/correcting-transfers.md`).
- **Privacy.** For GDPR, keep personal data out of TigerBeetle. Store only an opaque key in `user_data_128` and delete the mapping elsewhere (`debit-credit.md`, "Preserving User Privacy").

The global invariants (`reference/account.md`, "Guarantees") follow directly from the record shapes:

```
Σ debits_posted  == Σ credits_posted     (over all accounts)
Σ debits_pending == Σ credits_pending
```

Every code path adds or removes the *same* `amount` on one debit field and one credit field.

### 3.10 Validation order is part of the API

`CreateTransferStatus` (`src/tigerbeetle.zig:220`) is "ordered by descending precedence". When several errors apply, the first in declaration order wins. The numeric values are *not* the precedence; for example `exists = 46` is checked before `flags_are_mutually_exclusive = 7`. Success is `created = maxInt(u32)`.

Why this matters: two implementations with the same rules but a different check order return different codes. That breaks determinism across versions (Concept 20) and breaks client retry logic (Concept 4).

### Why enforce invariants in the database

- **One choke point.** Every writer goes through the same `create_transfer`. No service can forget a rule.
- **No check-then-act gap.** The check and the write happen in the same single-threaded step (Concept 6), with no lock held over the network (Concept 3).
- **Auditability.** Immutable, append-only history plus database-enforced balance rules means a client never observes a broken invariant (`reference/sessions.md`, "Guarantees").

## 4. What it means for ashi

**Layout.** Mirror the records exactly and let the compiler police them:

```rust
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct Account {
    pub id: u128,
    pub debits_pending: u128,
    pub debits_posted: u128,
    pub credits_pending: u128,
    pub credits_posted: u128,
    pub user_data_128: u128,
    pub user_data_64: u64,
    pub user_data_32: u32,
    pub reserved: u32,
    pub ledger: u32,
    pub code: u16,
    pub flags: AccountFlags, // #[repr(transparent)] struct AccountFlags(u16)
    pub timestamp: u64,
}
const _: () = assert!(core::mem::size_of::<Account>() == 128);
const _: () = assert!(core::mem::align_of::<Account>() == 16);
```

**Rust features that help:**
- `const` assertions replace Zig's `comptime` block, with no crate needed.
- Exhaustive `match` on a `#[repr(u32)] enum CreateTransferStatus` makes the compiler flag every unhandled status code.
- `checked_add` maps directly to `sum_overflows`, and `saturating_sub` to Zig's `-|`.

**Pitfalls specific to Rust:**
- **`u128` alignment changed.** Before Rust 1.77, `u128` was 8-byte aligned on x86-64 (unlike C's `__int128`); since 1.77 it is 16. Write `align(16)` explicitly and assert it. Don't depend on the toolchain version.
- **Never use `bool` fields in a wire struct.** A `bool` has invalid bit patterns (anything but 0/1), so zero-copy casting raw bytes to a struct containing `bool` is undefined behaviour. Use a `u16` newtype and validate padding bits, the way TigerBeetle returns `reserved_flag`.
- **Release builds wrap on overflow** unless `overflow-checks = true`. TigerBeetle's own Rust client `Cargo.toml` "strongly" recommends enabling it. In the state machine, use explicit `checked_*` calls anyway, so the behaviour is visible in the code.
- **Precedence ≠ discriminant.** Keep validation as one ordered function, and test the order explicitly.

**Patterns:**
- Store pending status in a separate small record (like `TransferPending`), so `Transfer` values stay truly immutable. Then they can be shared as `&Transfer` without fear.
- Implement linked-chain rollback as an undo log or scoped write buffer over the single-writer state. Don't clone maps.

**Crates worth evaluating** (TigerBeetle has zero dependencies, so each one is a deliberate choice; see Concept 13):
- `zerocopy` or `bytemuck` for safe byte-to-struct casts. Both derive-check "no padding", like `stdx.no_padding`.
- `bitflags` for flag types. It is fine, but a hand-written newtype with consts is about 20 lines, and it is what TigerBeetle's generated Rust client does (`src/clients/rust/src/tb_client.rs:9`).

**ashi stance:** _to be decided once the drill is done_
Suggestion: **adopt**. The debit/credit schema, its 128-byte layout and its invariants are the product's interface. Diverging buys little and costs compatibility.

## 5. Drill questions

### Q1. Recall

List the fields of `Transfer` with their sizes, and show they sum to 128 bytes. Which fields does the client leave as zero on a normal (non-imported) single-phase transfer?

> _Your answer:_

### Q2. Mechanism

Explain why `debits_exceed_credits` adds `debits_pending` but ignores `credits_pending`. What would go wrong if pending credits counted toward the available balance?

> _Your answer:_

### Q3. Back-of-envelope

Account `W` has `debits_must_not_exceed_credits`, `credits_posted = 500`, `debits_posted = 200`, `debits_pending = 150` (one pending transfer `P` of 150).

1. A new pending transfer debits `W` for 200. Result?
2. Instead, a single-phase `balancing_debit` transfer debits `W` with `amount = AMOUNT_MAX`. What amount is recorded?
3. After (2), `P` is posted with `amount = 100`. What are `W`'s four counters, and its available balance?

> _Your answer:_

### Q4. What breaks if…

…closing an account could be done by a plain single-phase transfer with a `closing_debit` flag, and reopening by a separate "reopen" flag?

> _Your answer:_

### Q5. ashi design (open-ended)

TigerBeetle transfers have exactly one debit and one credit. Multi-leg transactions are linked chains. Should ashi keep that, or add a native multi-leg transfer? Consider record size, cache lines (Concept 9), validation and idempotency.

> _Your answer:_

## Answer key

<details><summary>Answer key (try first)</summary>

**Q1.**
- `id` 16, `debit_account_id` 16, `credit_account_id` 16, `amount` 16, `pending_id` 16, `user_data_128` 16 (96 so far), `user_data_64` 8 (104), `user_data_32` 4 (108), `timeout` 4 (112), `ledger` 4 (116), `code` 2 (118), `flags` 2 (120), `timestamp` 8 = **128**.
- On a normal single-phase transfer the client leaves `pending_id = 0`, `timeout = 0` (otherwise `timeout_reserved_for_pending_transfer`), and `timestamp = 0` (otherwise `timestamp_must_be_zero`; the cluster assigns it).
- `user_data_*` may also be 0 (optional).
- Flags `pending`/`post`/`void`/`closing_*` are clear.

**Q2.**
- Pending debits are money promised *out*. If they didn't count, two pending transfers could each pass the check and together overdraw the account when both post.
- Pending credits are money that *might* arrive. They can still be voided or expire.
- If they counted, you could spend incoming money, then the incoming pending is voided, and the account is negative with nothing to stop it.
- The rule is pessimistic on both sides, and that is what guarantees that "post or void never breaks a limit" (`two-phase-transfers.md`).

**Q3.**
1. Check: `150 + 200 + 200 = 550 > 500`, so `exceeds_credits`. It fails at pending time, not at post time ("Pessimistic Pending Transfers").
2. `min(AMOUNT_MAX, 500 -| (200 + 150)) = 150`. Recorded amount: 150. Now `debits_posted = 350`, `debits_pending = 150`.
3. Posting `P` releases the full 150 from pending and posts 100:
   - `debits_pending = 0`, `debits_posted = 450`, `credits_pending = 0`, `credits_posted = 500`.
   - Available = `500 - 450 - 0 = 50`. The unposted 50 returned to the account.

**Q4.**
- A reopen that doesn't reference a specific close can interleave badly. Operator X closes the account for an audit, operator Y's stale "reopen" lands afterwards and silently undoes X's close.
- With a pending close, the reopen is a void that must name the closing transfer's `id` (`pending_id`), and a pending resolves at most once. So a reopen can only undo the exact close it refers to, exactly once.
- Pending closes also get expiry for free (temporary freezes), and the close/reopen history stays in the immutable log.
- The docs give this reason under `flags.closing_debit`: "guarding against unexpected interleaving of close/unclose operations."

**Q5.** Open-ended. Points a good answer weighs:
- **Fixed size.** A native multi-leg record is variable-size or has a fixed max of legs. Either breaks the 128-byte, two-cache-line object and complicates the LSM trees (fixed-size values, Concept 18).
- **Chains already give atomicity.** Linked chains provide atomic multi-leg semantics with no new record type. The cost is N records and N idempotency keys, one per leg.
- **Idempotency gets harder with chains.** A per-leg `exists` breaks a retried chain (see 3.6). A native multi-leg event would have one idempotency key, which is simpler to retry.
- **Validation complexity.** Σ debits = Σ credits within the event, plus per-leg limits.
- **A middle ground:** keep 1-debit/1-credit as the storage primitive, and add a client-side or gateway-side "transaction" abstraction that compiles to a linked chain with derived ids.

</details>

## Review notes

_Filled in after grading: what stuck, what to revisit._
