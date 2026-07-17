use crate::errors::ContractError;
use crate::helpers::{
    apply_milestone_bonus, calculate_daily_compound_interest, config, get_active_loan_record,
    has_active_loan, next_loan_id, require_allowed_token, require_not_paused,
};
use crate::reputation::ReputationNftExternalClient;
use crate::types::{
    DataKey, LoanRecord, LoanStatus, VouchRecord, DEFAULT_REFERRAL_BONUS_BPS, MIN_VOUCH_AGE,
    SECS_PER_DAY,
};
use soroban_sdk::{symbol_short, Address, Env, Vec};

/// Register a referrer for a borrower. Must be called before `request_loan`.
/// The referrer cannot be the borrower themselves.
pub fn register_referral(
    env: Env,
    borrower: Address,
    referrer: Address,
) -> Result<(), ContractError> {
    borrower.require_auth();
    require_not_paused(&env)?;

    assert!(borrower != referrer, "borrower cannot refer themselves");
    assert!(
        !has_active_loan(&env, &borrower),
        "cannot set referral with active loan"
    );
    // Idempotent: overwrite is fine (borrower signs).
    env.storage()
        .persistent()
        .set(&DataKey::ReferredBy(borrower.clone()), &referrer);

    env.events().publish(
        (symbol_short!("referral"), symbol_short!("set")),
        (borrower, referrer),
    );

    Ok(())
}

pub fn get_referrer(env: Env, borrower: Address) -> Option<Address> {
    env.storage()
        .persistent()
        .get(&DataKey::ReferredBy(borrower))
}

pub fn request_loan(
    env: Env,
    borrower: Address,
    amount: i128,
    threshold: i128,
    loan_purpose: soroban_sdk::String,
    token_addr: Address,
) -> Result<(), ContractError> {
    borrower.require_auth();
    require_not_paused(&env)?;

    if env
        .storage()
        .persistent()
        .get::<DataKey, bool>(&DataKey::Blacklisted(borrower.clone()))
        .unwrap_or(false)
    {
        return Err(ContractError::Blacklisted);
    }

    // Validate token is allowed before any other checks.
    let token_client = require_allowed_token(&env, &token_addr)?;

    let cfg = config(&env);

    assert!(
        amount >= cfg.min_loan_amount,
        "loan amount must meet minimum threshold"
    );
    assert!(threshold > 0, "threshold must be greater than zero");

    let max_loan_amount: i128 = env
        .storage()
        .instance()
        .get(&DataKey::MaxLoanAmount)
        .unwrap_or(0);
    if max_loan_amount > 0 && amount > max_loan_amount {
        return Err(ContractError::LoanExceedsMaxAmount);
    }

    assert!(
        !has_active_loan(&env, &borrower),
        "borrower already has an active loan"
    );

    let vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower.clone()))
        .unwrap_or(Vec::new(&env));

    // Only count vouches denominated in the requested token.
    let mut token_vouches: Vec<VouchRecord> = Vec::new(&env);
    for v in vouches.iter() {
        if v.token == token_addr {
            token_vouches.push_back(v);
        }
    }

    let mut total_stake: i128 = 0;
    for v in token_vouches.iter() {
        total_stake = total_stake
            .checked_add(v.stake)
            .ok_or(ContractError::StakeOverflow)?;
    }
    assert!(total_stake >= threshold, "insufficient trust stake");

    let min_vouchers: u32 = env
        .storage()
        .instance()
        .get(&DataKey::MinVouchers)
        .unwrap_or(0);
    if token_vouches.len() < min_vouchers {
        return Err(ContractError::InsufficientVouchers);
    }

    let now = env.ledger().timestamp();
    for v in token_vouches.iter() {
        if now < v.vouch_timestamp + MIN_VOUCH_AGE {
            return Err(ContractError::VouchTooRecent);
        }
    }

    let max_allowed_loan = total_stake * cfg.max_loan_to_stake_ratio as i128 / 100;
    assert!(
        amount <= max_allowed_loan,
        "loan amount exceeds maximum collateral ratio"
    );

    let contract_balance = token_client.balance(&env.current_contract_address());
    if contract_balance < amount {
        return Err(ContractError::InsufficientFunds);
    }

    let deadline = now + cfg.loan_duration;
    let loan_id = next_loan_id(&env);
    let total_yield = amount * cfg.yield_bps / 10_000;

    env.storage().persistent().set(
        &DataKey::Loan(loan_id),
        &LoanRecord {
            id: loan_id,
            borrower: borrower.clone(),
            co_borrowers: Vec::new(&env),
            amount,
            amount_repaid: 0,
            total_yield,
            repaid: false,
            defaulted: false,
            created_at: now,
            disbursement_timestamp: now,
            repayment_timestamp: None,
            deadline,
            loan_purpose,
            token_address: token_addr.clone(),
            // Interest tracking: start the clock at disbursement so elapsed days
            // are correctly computed on the first repayment call.
            last_interest_calc: now,
            accrued_interest: 0,
            milestone_bonus_applied: 0,
        },
    );
    env.storage()
        .persistent()
        .set(&DataKey::ActiveLoan(borrower.clone()), &loan_id);
    env.storage()
        .persistent()
        .set(&DataKey::LatestLoan(borrower.clone()), &loan_id);

    let count: u32 = env
        .storage()
        .persistent()
        .get(&DataKey::LoanCount(borrower.clone()))
        .unwrap_or(0);
    env.storage()
        .persistent()
        .set(&DataKey::LoanCount(borrower.clone()), &(count + 1));

    token_client.transfer(&env.current_contract_address(), &borrower, &amount);

    env.events().publish(
        (symbol_short!("loan"), symbol_short!("disbursed")),
        (borrower.clone(), amount, deadline, token_addr),
    );

    Ok(())
}

/// Repay part or all of an active loan.
///
/// ## Interest Accrual Pipeline (executed at the top of every call)
///
/// 1. Compute `days_elapsed` = whole days since `last_interest_calc`.
/// 2. Compute new interest = `calculate_daily_compound_interest(outstanding_principal, days_elapsed)`.
/// 3. Add to `loan.accrued_interest` and advance `loan.last_interest_calc`.
///
/// ## Milestone Bonuses
///
/// After accrual and *after* adding `payment` to `amount_repaid`, we check
/// whether any repayment milestone (25 %, 50 %, 75 %) has just been crossed
/// for the first time.  If so, a one-time discount is applied to
/// `accrued_interest` via `apply_milestone_bonus`.
///
/// ## Total Obligation
///
/// ```text
/// total_owed = amount + total_yield + accrued_interest (after bonus adjustments)
/// ```
///
/// A payment is valid when `0 < payment ≤ (total_owed - amount_repaid)`.
pub fn repay(env: Env, borrower: Address, payment: i128) -> Result<(), ContractError> {
    borrower.require_auth();
    require_not_paused(&env)?;

    let mut loan = get_active_loan_record(&env, &borrower)?;

    for cb in loan.co_borrowers.iter() {
        cb.require_auth();
    }

    if borrower != loan.borrower {
        return Err(ContractError::UnauthorizedCaller);
    }
    if loan.defaulted || loan.repaid {
        return Err(ContractError::NoActiveLoan);
    }

    assert!(!loan.defaulted, "loan already defaulted");
    assert!(!loan.repaid, "loan already repaid");
    assert!(
        env.ledger().timestamp() <= loan.deadline,
        "loan deadline has passed"
    );

    // ── Step 1: Accrue compound interest ─────────────────────────────────────
    let now = env.ledger().timestamp();
    let elapsed_secs = now.saturating_sub(loan.last_interest_calc);
    let days_elapsed = elapsed_secs / SECS_PER_DAY;

    if days_elapsed > 0 {
        // Outstanding principal is everything not yet repaid, minus the static
        // yield component (which is already accounted for in total_yield).
        let outstanding_principal = (loan.amount - loan.amount_repaid).max(0);
        let new_interest =
            calculate_daily_compound_interest(outstanding_principal, days_elapsed);
        loan.accrued_interest = loan
            .accrued_interest
            .checked_add(new_interest)
            .unwrap_or(i128::MAX);
        // Advance the clock by whole days only; any sub-day remainder rolls
        // forward and will be picked up on the next call.
        loan.last_interest_calc += days_elapsed * SECS_PER_DAY;
    }

    // ── Step 2: Validate payment against total obligation ────────────────────
    //
    // total_owed = principal + static_yield + accrued_compound_interest
    let total_owed = loan
        .amount
        .checked_add(loan.total_yield)
        .and_then(|v| v.checked_add(loan.accrued_interest))
        .expect("total_owed overflow");

    let outstanding = total_owed
        .checked_sub(loan.amount_repaid)
        .unwrap_or(0)
        .max(0);

    assert!(
        payment > 0 && payment <= outstanding,
        "invalid payment amount"
    );

    // ── Step 3: Apply payment ─────────────────────────────────────────────────
    let token = soroban_sdk::token::Client::new(&env, &loan.token_address);
    token.transfer(&borrower, &env.current_contract_address(), &payment);
    loan.amount_repaid = loan
        .amount_repaid
        .checked_add(payment)
        .expect("amount_repaid overflow");

    // ── Step 4: Check milestones (post-payment) ───────────────────────────────
    // total_obligation for milestone fraction: principal + static yield only.
    // (We exclude accrued_interest from the denominator so early repayers
    // aren't penalised by a growing denominator.)
    let total_obligation_for_milestone = loan
        .amount
        .checked_add(loan.total_yield)
        .unwrap_or(loan.amount);

    let (new_accrued, new_flags) = apply_milestone_bonus(
        &loan,
        loan.amount_repaid,
        total_obligation_for_milestone,
    );
    loan.accrued_interest = new_accrued;
    loan.milestone_bonus_applied = new_flags;

    // ── Step 5: Re-check whether fully repaid (with updated accrued_interest) ─
    let total_owed_final = loan
        .amount
        .checked_add(loan.total_yield)
        .and_then(|v| v.checked_add(loan.accrued_interest))
        .expect("total_owed_final overflow");

    let fully_repaid = loan.amount_repaid >= total_owed_final;

    if fully_repaid {
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));
        // Only distribute yield to vouches in the same token as the loan.
        let loan_token = soroban_sdk::token::Client::new(&env, &loan.token_address);
        let mut total_stake: i128 = 0;
        for v in vouches.iter() {
            if v.token == loan.token_address {
                total_stake += v.stake;
            }
        }

        for v in vouches.iter() {
            if v.token != loan.token_address {
                continue;
            }
            let voucher_yield = if total_stake > 0 {
                loan.total_yield * v.stake / total_stake
            } else {
                0
            };
            loan_token.transfer(
                &env.current_contract_address(),
                &v.voucher,
                &(v.stake + voucher_yield),
            );
        }

        loan.repaid = true;
        loan.repayment_timestamp = Some(env.ledger().timestamp());

        // Pay referral bonus if a referrer is registered.
        if let Some(referrer) = env
            .storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::ReferredBy(borrower.clone()))
        {
            let bonus_bps: u32 = env
                .storage()
                .instance()
                .get(&DataKey::ReferralBonusBps)
                .unwrap_or(DEFAULT_REFERRAL_BONUS_BPS);
            let bonus = loan.amount * bonus_bps as i128 / 10_000;
            if bonus > 0 {
                loan_token.transfer(&env.current_contract_address(), &referrer, &bonus);
                env.events().publish(
                    (symbol_short!("referral"), symbol_short!("bonus")),
                    (referrer, borrower.clone(), bonus),
                );
            }
        }

        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::RepaymentCount(borrower.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::RepaymentCount(borrower.clone()), &(count + 1));

        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftExternalClient::new(&env, &nft_addr).mint(&borrower);
        }

        env.storage()
            .persistent()
            .remove(&DataKey::ActiveLoan(borrower.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower.clone()));

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("repaid")),
            (borrower.clone(), loan.amount),
        );
    }

    env.storage()
        .persistent()
        .set(&DataKey::Loan(loan.id), &loan);

    Ok(())
}

pub fn loan_status(env: Env, borrower: Address) -> LoanStatus {
    match crate::helpers::get_latest_loan_record(&env, &borrower) {
        None => LoanStatus::None,
        Some(loan) if loan.repaid => LoanStatus::Repaid,
        Some(loan) if loan.defaulted => LoanStatus::Defaulted,
        _ => LoanStatus::Active,
    }
}

pub fn get_loan(env: Env, borrower: Address) -> Option<LoanRecord> {
    crate::helpers::get_latest_loan_record(&env, &borrower)
}

pub fn get_loan_by_id(env: Env, loan_id: u64) -> Option<LoanRecord> {
    env.storage().persistent().get(&DataKey::Loan(loan_id))
}

pub fn is_eligible(env: Env, borrower: Address, threshold: i128) -> bool {
    if threshold <= 0 {
        return false;
    }

    if let Some(loan) = crate::helpers::get_latest_loan_record(&env, &borrower) {
        if !loan.repaid && !loan.defaulted {
            return false;
        }
    }

    let vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower))
        .unwrap_or(Vec::new(&env));

    let total_stake: i128 = vouches.iter().map(|v| v.stake).sum();
    total_stake >= threshold
}

pub fn repayment_count(env: Env, borrower: Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::RepaymentCount(borrower))
        .unwrap_or(0)
}

pub fn loan_count(env: Env, borrower: Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::LoanCount(borrower))
        .unwrap_or(0)
}

pub fn default_count(env: Env, borrower: Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::DefaultCount(borrower))
        .unwrap_or(0)
}
