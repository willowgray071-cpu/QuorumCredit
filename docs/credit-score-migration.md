# Credit Score Migration and Backfill Strategy

## Overview

The credit score system now tracks real repayment behavior. However, existing borrowers with loans pre-dating the implementation update will have no historical payment data available in the new payment tracking system.

## Current State of Historical Data

### Pre-Upgrade Loans (Problem)
- **No PaymentHistory records**: The new `PaymentHistory(loan_id)` storage key is empty for loans created before this upgrade
- **Available data**: Only the loan record itself is preserved, with fields:
  - `amount`: Principal borrowed
  - `amount_repaid`: Cumulative repayment (only final value)
  - `repayment_timestamp`: When fully repaid (if status = Repaid)
  - `disbursement_timestamp`: When disbursed
  - `deadline`: Loan deadline
  - `status`: Repaid, Defaulted, or Active

### Post-Upgrade Loans (Solved)
- **Complete tracking**: All new loans automatically have `PaymentHistory(loan_id)` records for each repayment
- **Timeliness calculated correctly**: Credits scores use real repayment timeliness going forward

## Migration Strategy

### Phase 1: Current Behavior (No Backfill)
Until backfill is implemented, credit scores use only post-upgrade data:

1. **New borrowers** (no pre-upgrade loans):
   - Start with neutral score (500) when first loan is repaid
   - Score improves/degrades with actual repayment history

2. **Existing borrowers** (with pre-upgrade loans):
   - Their aggregates are now calculated from loan records:
     - `total_borrowed`: Sum of `loan.amount` for all loans
     - `total_repaid`: Sum of `loan.amount_repaid` for all loans
   - However, `avg_repayment_time` starts at 0 (neutral):
     - Pre-upgrade loans have no payment timestamp granularity
     - Only the final `repayment_timestamp` is known
   - **Result**: Credit score reflects on-time/late status of older loans but not granular timeliness
   - **Workaround for borrowers**: They can immediately re-borrow and re-repay post-upgrade to establish fresh payment history with precise timeliness tracking

### Phase 2: Optional Backfill (Post-Deployment)

If historical payment data is available from an off-chain source or contract logs, implement:

```rust
/// One-time backfill of payment history for pre-upgrade loans
/// Called by admin to populate historical payment records
pub fn backfill_payment_history(
    env: Env,
    admin_signers: Vec<Address>,
    loan_id: u64,
    payment_records: Vec<PaymentRecord>,
) -> Result<(), ContractError> {
    // 1. Verify admin authorization
    // 2. Load the loan record
    // 3. If status is Repaid or Defaulted (terminal state):
    //    - Append payment_records to PaymentHistory(loan_id)
    //    - Recalculate timeliness scores for all borrowers
    // 4. Emit event for audit trail
}

/// Batch backfill for multiple pre-upgrade loans
pub fn backfill_payment_history_batch(
    env: Env,
    admin_signers: Vec<Address>,
    backfills: Vec<(u64, Vec<PaymentRecord>)>,
) -> Result<(), ContractError> {
    // Similar to above, but in a single transaction
}
```

**Data Requirements**:
- Loan ID
- Per-payment: `(amount, timestamp, cumulative_repaid)` tuples
- Source: Contract event logs, off-chain database, or audit trail

**Validation**:
- `cumulative_repaid` must be monotonically increasing
- Last `cumulative_repaid` must match `loan.amount_repaid`
- All timestamps must be between `disbursement_timestamp` and `repayment_timestamp`

### Phase 3: Recalculation (After Backfill)

Once backfill is complete:

```rust
/// Recalculate credit scores for all borrowers after historical backfill
pub fn recalculate_all_credit_scores(
    env: Env,
    admin_signers: Vec<Address>,
) -> Result<(), ContractError> {
    // Iterate over BorrowerList
    // Call update_credit_score() for each borrower
    // New scores now reflect accurate historical timeliness
}
```

## Impact on Credit Scores

### Before Backfill (Current)
```
Borrower A (pre-upgrade, 5 loans, all repaid on-time):
- total_borrowed: 5M stroops ✓
- total_repaid: 5M stroops ✓
- avg_repayment_time: 0 (neutral) ✗
- Effective Score: ~600-700 (missing timeliness boost)

Borrower B (post-upgrade, 1 loan, repaid early):
- total_borrowed: 1M stroops ✓
- total_repaid: 1M stroops ✓
- avg_repayment_time: +432000 (5 days early) ✓
- Effective Score: ~750+ (with timeliness boost)
```

### After Backfill (Recommended)
```
Borrower A (with historical data):
- total_borrowed: 5M stroops ✓
- total_repaid: 5M stroops ✓
- avg_repayment_time: +86400 (1 day early, average) ✓
- Effective Score: ~750+ (matches actual behavior)
```

## Recommended Actions for Admins

### Immediate (Day 1)
1. **Deploy** the upgraded contract with real credit score tracking
2. **Disable** credit score requirements (if any) during initial grace period
3. **Monitor** new loan originations to verify correct scoring

### Short-term (Week 1)
1. **Notify borrowers** of the change
2. **Document** that pre-upgrade loans may have conservative timeliness scores
3. **Provide guidance**: Borrowers can re-borrow/re-repay to establish fresh history if needed

### Medium-term (Weeks 2-4)
1. **Collect** historical payment data from logs or audit trail
2. **Validate** data completeness and accuracy
3. **Prepare** backfill payloads with payment records

### Long-term (Months 1-3)
1. **Execute** backfill batches (if data is available)
2. **Recalculate** all credit scores
3. **Monitor** for score stability

## Example Backfill Process

### Step 1: Retrieve Historical Payments from Logs
```bash
# Query contract event logs for all "loan/repay" events
# Extract: (borrower, loan_id, payment_amount, timestamp)
# Group by loan_id, sorted by timestamp
```

### Step 2: Construct Payment Records
```rust
let historical_payments = vec![
    PaymentRecord {
        amount: 100_000,
        timestamp: 1700000000,
        cumulative_repaid: 100_000,
    },
    PaymentRecord {
        amount: 200_000,
        timestamp: 1700100000,
        cumulative_repaid: 300_000,
    },
    PaymentRecord {
        amount: 400_000,
        timestamp: 1700200000,
        cumulative_repaid: 700_000,
    },
];
```

### Step 3: Backfill and Verify
```rust
// Call backfill function for loan_id = 123
backfill_payment_history(env, admin_signers, 123, historical_payments)?;

// Verify by loading payment history
let history: Vec<PaymentRecord> = env
    .storage()
    .persistent()
    .get(&DataKey::PaymentHistory(123))
    .unwrap();

assert_eq!(history.len(), 3);
assert_eq!(history.last().unwrap().cumulative_repaid, 700_000);
```

### Step 4: Recalculate Scores
```rust
recalculate_all_credit_scores(env, admin_signers)?;

// Scores are now updated with historical timeliness data
```

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| **Missing payment data** | Incomplete backfill leads to incorrect scores | Validate against on-chain loan records before backfill |
| **Timestamp accuracy** | Off-by-one errors in timeliness calculation | Cross-reference with contract timestamps from logs |
| **Data integrity** | Corrupted backfill affects multiple borrowers | Use test backfill on subset before full batch |
| **Score volatility** | Large score changes after backfill may surprise users | Announce migration plan in advance; consider soft transition |

## Testing

Run the migration-related tests:
```bash
# Test the credit score calculation with historical data
cargo test credit_score_test::test_credit_score_migration_strategy_note

# Test aggregate calculations (pre-backfill state)
cargo test credit_score_test::test_credit_score_total_borrowed
cargo test credit_score_test::test_credit_score_total_repaid

# Test timeliness calculations (post-backfill state)
cargo test credit_score_test::test_timeliness_score_*
```

## FAQ

**Q: Do existing borrowers' scores change immediately after the upgrade?**
A: Yes, because `total_borrowed` and `total_repaid` are now calculated from loan records. However, `avg_repayment_time` will be 0 (neutral) until backfill is complete or new loans are repaid.

**Q: What should a borrower do if their score seems lower than expected?**
A: Their historical payment timeliness may not be fully tracked yet. They can:
1. Wait for admin backfill (if available)
2. Request a new loan post-upgrade and repay early to establish fresh history
3. Contact protocol admins for backfill status

**Q: How long does backfill take?**
A: Depends on data availability and batch size. A batch of 100-1000 loans can typically be backfilled in a single transaction or a few transactions.

**Q: Can backfill be partial (some loans, not all)?**
A: Yes. Backfill can target specific loans or borrowers if only partial data is available. Unbackfilled loans will continue using current scoring logic.

**Q: Is backfill reversible?**
A: Yes. `PaymentHistory` records can be cleared and recalculated. However, once public credit scores are based on them, users may question sudden changes.
