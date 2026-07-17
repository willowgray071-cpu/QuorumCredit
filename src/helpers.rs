use crate::errors::ContractError;
use crate::types::{
    Config, DataKey, LoanRecord, COMPOUND_RATE_BPS, MILESTONE_25_DISCOUNT_BPS,
    MILESTONE_25_PCT_PERMILLE, MILESTONE_50_DISCOUNT_BPS, MILESTONE_50_PCT_PERMILLE,
    MILESTONE_75_DISCOUNT_BPS, MILESTONE_75_PCT_PERMILLE, MILESTONE_FLAG_25, MILESTONE_FLAG_50,
    MILESTONE_FLAG_75, SECS_PER_DAY,
};
use soroban_sdk::{token, Address, Env, String, Vec};

/// Returns true if the address is the all-zeros account or contract address.
pub fn is_zero_address(env: &Env, addr: &Address) -> bool {
    // Stellar zero account: all-zero 32-byte ed25519 key
    let zero_account = Address::from_string(&String::from_str(
        env,
        "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
    ));
    // Stellar zero contract: all-zero 32-byte contract hash
    let zero_contract = Address::from_string(&String::from_str(
        env,
        "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
    ));
    addr == &zero_account || addr == &zero_contract
}

pub fn require_not_paused(env: &Env) -> Result<(), ContractError> {
    let paused: bool = env
        .storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false);
    if paused {
        Err(ContractError::ContractPaused)
    } else {
        Ok(())
    }
}

/// Returns `Err(InsufficientFunds)` if `amount` is not strictly positive (≤ 0).
/// Use this for all numeric inputs that must be > 0 (stakes, loan amounts, thresholds).
pub fn require_positive_amount(_env: &Env, amount: i128) -> Result<(), ContractError> {
    if amount <= 0 {
        return Err(ContractError::InsufficientFunds);
    }
    Ok(())
}

pub fn config(env: &Env) -> Config {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .expect("not initialized")
}

pub fn add_slash_balance(env: &Env, amount: i128) {
    let current: i128 = env
        .storage()
        .instance()
        .get(&DataKey::SlashTreasury)
        .unwrap_or(0);
    env.storage()
        .instance()
        .set(&DataKey::SlashTreasury, &(current + amount));
}

pub fn has_active_loan(env: &Env, borrower: &Address) -> bool {
    matches!(get_active_loan_record(env, borrower), Ok(loan) if !loan.repaid && !loan.defaulted)
}

pub fn next_loan_id(env: &Env) -> u64 {
    let loan_id = env
        .storage()
        .instance()
        .get(&DataKey::LoanCounter)
        .unwrap_or(0u64)
        .checked_add(1)
        .expect("loan ID overflow");
    env.storage()
        .instance()
        .set(&DataKey::LoanCounter, &loan_id);
    loan_id
}

pub fn get_active_loan_record(env: &Env, borrower: &Address) -> Result<LoanRecord, ContractError> {
    let loan_id: u64 = env
        .storage()
        .persistent()
        .get(&DataKey::ActiveLoan(borrower.clone()))
        .ok_or(ContractError::NoActiveLoan)?;
    env.storage()
        .persistent()
        .get(&DataKey::Loan(loan_id))
        .ok_or(ContractError::NoActiveLoan)
}

pub fn get_latest_loan_record(env: &Env, borrower: &Address) -> Option<LoanRecord> {
    let loan_id: u64 = env
        .storage()
        .persistent()
        .get(&DataKey::LatestLoan(borrower.clone()))?;
    env.storage().persistent().get(&DataKey::Loan(loan_id))
}

pub fn token(env: &Env) -> token::Client<'_> {
    let addr = config(env).token;
    token::Client::new(env, &addr)
}

pub fn token_client(env: &Env) -> token::Client<'_> {
    token(env)
}

/// Returns a token client for `addr` after verifying it is an allowed token
/// (either the primary protocol token or in `Config.allowed_tokens`).
pub fn require_allowed_token<'a>(
    env: &'a Env,
    addr: &Address,
) -> Result<token::Client<'a>, ContractError> {
    let cfg = config(env);
    if *addr == cfg.token || cfg.allowed_tokens.iter().any(|t| t == *addr) {
        Ok(token::Client::new(env, addr))
    } else {
        Err(ContractError::InvalidToken)
    }
}

pub fn require_admin_approval(env: &Env, admin_signers: &Vec<Address>) {
    let config = config(env);
    assert!(
        admin_signers.len() >= config.admin_threshold,
        "insufficient admin approvals"
    );
    for signer in admin_signers.iter() {
        assert!(
            config.admins.iter().any(|a| a == signer),
            "signer is not a registered admin"
        );
        signer.require_auth();
    }
}

/// Validates that an address is not a zero address
pub fn require_valid_address(env: &Env, addr: &Address) -> Result<(), ContractError> {
    if is_zero_address(env, addr) {
        Err(ContractError::ZeroAddress)
    } else {
        Ok(())
    }
}

/// Validates that an address implements the SEP-41 token interface by attempting
/// to call `balance()` on it. A plain account address will cause a host trap,
/// which we catch via `try_invoke` semantics using the token client's try_ variant.
pub fn require_valid_token(env: &Env, addr: &Address) -> Result<(), ContractError> {
    require_valid_address(env, addr)?;
    // Attempt to call balance() on the address. If it's not a token contract,
    // the invocation will fail and we return InvalidToken.
    let client = token::Client::new(env, addr);
    // Use a dummy address (the contract itself) — we only care whether the call
    // succeeds, not the returned value.
    let probe = env.current_contract_address();
    if client.try_balance(&probe).is_err() {
        return Err(ContractError::InvalidToken);
    }
    Ok(())
}

pub fn validate_admin_config(
    env: &Env,
    admins: &Vec<Address>,
    admin_threshold: u32,
) -> Result<(), ContractError> {
    assert!(!admins.is_empty(), "at least one admin is required");
    assert!(
        admin_threshold > 0,
        "admin threshold must be greater than zero"
    );
    assert!(
        admin_threshold <= admins.len(),
        "admin threshold cannot exceed admin count"
    );

    let admin_count = admins.len();
    for i in 0..admin_count {
        let admin = admins.get(i).unwrap();

        // Validate admin address is not zero
        require_valid_address(env, &admin)?;

        // Check for duplicates
        for j in 0..i {
            let prior_admin = admins.get(j).unwrap();
            assert!(admin != prior_admin, "duplicate admin");
        }
    }

    Ok(())
}

// ── Daily-Compound Interest ───────────────────────────────────────────────────

/// Calculate compound interest that has accrued over `days_elapsed` whole days
/// on `outstanding_principal` at the annual rate `COMPOUND_RATE_BPS`.
///
/// Formula (all integer arithmetic, truncating):
/// ```text
/// interest_per_day = outstanding_principal * COMPOUND_RATE_BPS / 10_000 / 365
/// total            = interest_per_day * days_elapsed
/// ```
///
/// Notes:
/// - Returns 0 when `days_elapsed == 0` (same-day calls never double-charge).
/// - Returns 0 when `outstanding_principal <= 0` (defensive guard).
/// - The result is always ≥ 0.
pub fn calculate_daily_compound_interest(
    outstanding_principal: i128,
    days_elapsed: u64,
) -> i128 {
    if outstanding_principal <= 0 || days_elapsed == 0 {
        return 0;
    }
    // interest_per_day = principal * COMPOUND_RATE_BPS / (10_000 * 365)
    // We compute it as two separate divisions to stay within i128 for realistic
    // Soroban loan sizes (principal ≤ 10^18 stroops is safe).
    let daily_interest = outstanding_principal
        .checked_mul(COMPOUND_RATE_BPS)
        .unwrap_or(0)
        / 10_000
        / 365;

    daily_interest
        .checked_mul(days_elapsed as i128)
        .unwrap_or(0)
}

// ── Milestone Bonus Application ───────────────────────────────────────────────

/// Check whether any new repayment milestone has been crossed by the borrower
/// and, if so, apply a one-time discount to `accrued_interest` (floor 0).
///
/// Milestones are checked in ascending order (25 % → 50 % → 75 %).  Each fires
/// at most once per loan, tracked by the `milestone_bonus_applied` bitmask on
/// the `LoanRecord`.
///
/// `amount_repaid_after_payment` is the *new* cumulative repaid amount after
/// the current payment is applied.  `total_obligation` is the full amount owed
/// (principal + static yield; compound interest is handled separately).
///
/// Returns the updated `(accrued_interest, milestone_bonus_applied)`.
pub fn apply_milestone_bonus(
    loan: &LoanRecord,
    amount_repaid_after_payment: i128,
    total_obligation: i128,
) -> (i128, u32) {
    if total_obligation <= 0 {
        return (loan.accrued_interest, loan.milestone_bonus_applied);
    }

    let mut accrued = loan.accrued_interest;
    let mut flags = loan.milestone_bonus_applied;

    // Compute the repaid fraction in per-mille (‰) to avoid floating-point.
    // repaid_permille = amount_repaid_after_payment * 1000 / total_obligation
    let repaid_permille: u32 = (amount_repaid_after_payment
        .saturating_mul(1_000)
        / total_obligation)
        .max(0) as u32;

    // 75 % milestone — must be applied before 50 % / 25 % to avoid applying
    // lower-tier discounts to an already-reduced balance.
    if repaid_permille >= MILESTONE_75_PCT_PERMILLE && (flags & MILESTONE_FLAG_75 == 0) {
        let discount = accrued
            .saturating_mul(MILESTONE_75_DISCOUNT_BPS)
            / 10_000;
        accrued = (accrued - discount).max(0);
        flags |= MILESTONE_FLAG_75;
    }

    // 50 % milestone
    if repaid_permille >= MILESTONE_50_PCT_PERMILLE && (flags & MILESTONE_FLAG_50 == 0) {
        let discount = accrued
            .saturating_mul(MILESTONE_50_DISCOUNT_BPS)
            / 10_000;
        accrued = (accrued - discount).max(0);
        flags |= MILESTONE_FLAG_50;
    }

    // 25 % milestone
    if repaid_permille >= MILESTONE_25_PCT_PERMILLE && (flags & MILESTONE_FLAG_25 == 0) {
        let discount = accrued
            .saturating_mul(MILESTONE_25_DISCOUNT_BPS)
            / 10_000;
        accrued = (accrued - discount).max(0);
        flags |= MILESTONE_FLAG_25;
    }

    (accrued, flags)
}
