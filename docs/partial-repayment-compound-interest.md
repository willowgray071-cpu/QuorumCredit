# Partial Repayment & Daily-Compound Interest

> This document describes the **actual, shipped** interest model in QuorumCredit.
> It supersedes any prior draft that described a design which was not yet
> implemented.
>
> In this repository the public repayment entrypoint is `repay()`; there is no
> separate `repay_partial()` or `process_partial_repayment()` implementation.
> The daily accrual pipeline runs at the start of `repay()` for both full and
> partial repayments.

---

## Overview

QuorumCredit charges two distinct components of interest on active loans:

| Component | Field | Set when | Updated when |
|---|---|---|---|
| **Static yield** | `total_yield` | Loan disbursement | Never (immutable) |
| **Compound interest** | `accrued_interest` | 0 at disbursement | Every `repay()` call |

The total amount a borrower must repay to close the loan is:

```
total_owed = amount + total_yield + accrued_interest
```

`total_yield` compensates vouchers for their locked capital and is fixed at
disbursement from `Config::yield_bps` (default 200 bps = 2%).  `accrued_interest`
grows daily on the outstanding principal and is the mechanism that penalises
slow repayment.

---

## Daily-Compound Interest

### Tracking Fields on `LoanRecord`

| Field | Type | Description |
|---|---|---|
| `last_interest_calc` | `u64` | Ledger timestamp of the last accrual. Initialised to `disbursement_timestamp`. |
| `accrued_interest` | `i128` | Total compound interest accrued so far but not yet repaid. |

### Formula

Every `repay()` call runs the accrual pipeline **before** validating or applying
the payment:

```
elapsed_secs  = now - last_interest_calc
days_elapsed  = elapsed_secs / 86_400          (integer, truncating — whole days only)
daily_rate    = outstanding_principal * COMPOUND_RATE_BPS / 10_000 / 365
new_interest  = daily_rate * days_elapsed

accrued_interest   += new_interest
last_interest_calc += days_elapsed * 86_400    (advance by whole days, not elapsed_secs)
```

The remainder of any partial day rolls forward to the next call.

### Constants

| Constant | Value | Meaning |
|---|---|---|
| `SECS_PER_DAY` | `86_400` | Seconds in one day |
| `COMPOUND_RATE_BPS` | `500` | Annual interest rate in basis points (5% p.a.) |

### Worked Example

Loan of **100,000 stroops** at `yield_bps = 200`, outstanding for **30 days**
before any payment:

```
static_yield       = 100_000 * 200 / 10_000          = 2_000 stroops
daily_rate         = 100_000 * 500 / 10_000 / 365     = 136 stroops/day  (truncated)
accrued_interest   = 136 * 30                          = 4_080 stroops

total_owed         = 100_000 + 2_000 + 4_080           = 106_080 stroops
```

### Same-Day Repayments

When `days_elapsed == 0`, the accrual step adds zero interest.  Multiple
repayments on the same ledger day are safe: no double-charging occurs.

---

## Milestone Bonuses

Milestone bonuses reward early repayment by reducing the remaining
`accrued_interest`.  Each bonus fires at most once per loan, tracked by a
bitmask in `LoanRecord::milestone_bonus_applied`.

### Thresholds & Discounts

The fraction repaid is measured against `amount + total_yield` (principal plus
static yield — the denominator is fixed and never inflated by `accrued_interest`,
so borrowers are not penalised for accruing interest).

| Milestone | Fraction repaid | Bit flag | Discount on `accrued_interest` |
|---|---|---|---|
| 25% | ≥ 250‰ of obligation | `MILESTONE_FLAG_25` (bit 0) | 10% (`1_000 bps`) |
| 50% | ≥ 500‰ of obligation | `MILESTONE_FLAG_50` (bit 1) | 20% (`2_000 bps`) |
| 75% | ≥ 750‰ of obligation | `MILESTONE_FLAG_75` (bit 2) | 30% (`3_000 bps`) |

### Ordering

Milestones are evaluated **highest-first** (75 % → 50 % → 25 %) within a single
`repay()` call.  This ensures that when a borrower skips straight to 75%, all
three bonuses fire in the correct sequence without one tier's discount being
double-applied to a balance already reduced by a higher tier.

### Worked Example

`accrued_interest = 10_000`, borrower repays 80% of obligation in one call:

```
After 75% bonus (30%): 10_000 - 3_000 = 7_000
After 50% bonus (20%):  7_000 - 1_400 = 5_600
After 25% bonus (10%):  5_600 -   560 = 5_040
```

The floor is 0 — `accrued_interest` never goes negative.

---

## Repayment Validation

After the interest accrual and milestone check, the payment is validated:

```
total_owed   = amount + total_yield + accrued_interest   (post-milestone value)
outstanding  = total_owed - amount_repaid
assert 0 < payment ≤ outstanding
```

### Fully Repaid

When `amount_repaid >= total_owed`, the loan is marked `repaid = true`,
vouchers receive their stake plus their proportional share of `total_yield`,
and the reputation NFT is minted.

---

## Dynamic Yield & Reputation

The current contract does not implement a runtime `calculate_dynamic_yield`
function that calls an external reputation oracle during repayment.  Instead,
`repay()` uses the deterministic daily-compounding pipeline described above.

That design was chosen because:

1. The production `ReputationNftContract` is already implemented in
   [src/reputation.rs](src/reputation.rs) and can be deployed independently.
2. Compound interest provides a deterministic, on-chain penalty for slow
   repayment without adding extra cross-contract calls to the repayment path.
3. Any future reputation-adjusted yield mechanism would be a governance-driven
   change to configuration (`Config::yield_bps`) rather than a hidden runtime
   oracle dependency.

---

## `LoanRecord` Field Reference

```rust
pub struct LoanRecord {
    // ... (existing fields) ...

    /// Ledger timestamp of the last interest accrual.
    /// Initialised to `disbursement_timestamp`.
    pub last_interest_calc: u64,

    /// Total compound interest accrued but not yet repaid.
    /// Updated on every `repay()` call before the payment is applied.
    pub accrued_interest: i128,

    /// Bitmask: bit 0 = 25% milestone, bit 1 = 50%, bit 2 = 75%.
    /// Once set, never cleared — each bonus fires at most once per loan.
    pub milestone_bonus_applied: u32,
}
```

---

## Interaction with Other Features

### Referral Bonus
The referral bonus (`ReferralBonusBps`) is paid out of the contract's yield
reserve when the loan is **fully repaid**.  It is calculated on the original
`loan.amount` (principal only) and is independent of `accrued_interest`.

### Slash / Default
When a loan defaults (`slash`, `auto_slash`, `claim_expired_loan`), the
`accrued_interest` field is neither charged nor refunded — the slash penalty
is applied to voucher stakes as normal.  Outstanding interest is simply
forgiven on default.

### Loan Pools
Loans created via `create_loan_pool` use the same `LoanRecord` struct and the
same `repay()` pipeline.  Interest accrues identically for pool loans.

---

## Test Coverage

Property tests live in `src/interest_test.rs` and cover:

- **Pure unit tests** (`calculate_daily_compound_interest`):
  - Zero days → zero interest
  - Zero/negative principal → zero interest
  - Known 1-day value (1_000_000 stroops @ 500 bps/yr = 136 stroops/day)
  - 30-day = 30 × 1-day
  - 365-day ≈ annual rate (within integer rounding)
  - Large principal does not overflow

- **Pure unit tests** (`apply_milestone_bonus`):
  - No bonus fires below 25%
  - Each milestone fires exactly once
  - All three fire in one call (correct ordering and compounding)
  - Accrued interest floored at 0

- **Integration tests** (via contract client, full ledger time):
  - Same-day repayment: zero interest
  - Two same-day repayments: no double-charging
  - 30-day gap: correct value
  - 365-day gap: near annual rate
  - Sequential partials accumulate correctly
  - Sub-day remainder truncated (whole-day granularity)
  - 25%/50%/75% milestones fire exactly once each
  - 730-day gap: no overflow for realistic loan sizes
