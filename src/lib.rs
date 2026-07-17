#![no_std]

use soroban_sdk::{
    contract, contractimpl, panic_with_error, symbol_short, Address, BytesN, Env, Vec,
};

pub mod admin;
pub mod errors;
pub mod governance;
pub mod helpers;
pub mod loan;
pub mod reputation;
pub mod types;
pub mod vouch;

#[cfg(test)]
mod governance_test;
#[cfg(test)]
mod interest_test;
#[cfg(test)]
mod loan_purpose_test;
#[cfg(test)]
mod multi_asset_test;
#[cfg(test)]
mod referral_test;

pub use errors::ContractError;
pub use types::*;

use helpers::{config, require_admin_approval, require_not_paused, require_valid_token,
              token, token_client, validate_admin_config};
use reputation::ReputationNftExternalClient;

#[contract]
pub struct QuorumCreditContract;

#[contractimpl]
impl QuorumCreditContract {
    // ── Initialization ────────────────────────────────────────────────────────

    pub fn initialize(
        env: Env,
        deployer: Address,
        admins: Vec<Address>,
        admin_threshold: u32,
        token_addr: Address,
    ) {
        deployer.require_auth();

        assert!(
            !env.storage().instance().has(&DataKey::Config),
            "already initialized"
        );

        validate_admin_config(&env, &admins, admin_threshold)
            .expect("invalid admin config");

        // Validate token address implements SEP-41.
        require_valid_token(&env, &token_addr).expect("invalid token address");

        env.storage().instance().set(&DataKey::Deployer, &deployer);
        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admins: admins.clone(),
                admin_threshold,
                token: token_addr.clone(),
                allowed_tokens: Vec::new(&env),
                yield_bps: DEFAULT_YIELD_BPS,
                slash_bps: DEFAULT_SLASH_BPS,
                max_vouchers: DEFAULT_MAX_VOUCHERS,
                min_loan_amount: DEFAULT_MIN_LOAN_AMOUNT,
                loan_duration: DEFAULT_LOAN_DURATION,
                max_loan_to_stake_ratio: DEFAULT_MAX_LOAN_TO_STAKE_RATIO,
                grace_period: 0,
                vouch_cooldown_secs: DEFAULT_VOUCH_COOLDOWN_SECS,
                min_yield_stake: DEFAULT_MIN_YIELD_STAKE,
            },
        );

        env.events().publish(
            (symbol_short!("contract"), symbol_short!("init")),
            (deployer, admins, admin_threshold, token_addr),
        );
    }

    // ── Slash ─────────────────────────────────────────────────────────────────

    /// Admin marks a loan defaulted; slash_bps% of each voucher's stake is slashed.
    pub fn slash(env: Env, admin_signers: Vec<Address>, borrower: Address) {
        require_admin_approval(&env, &admin_signers);
        require_not_paused(&env).expect("contract is paused");

        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::ActiveLoan(borrower.clone()))
            .and_then(|loan_id: u64| env.storage().persistent().get(&DataKey::Loan(loan_id)))
            .expect("no active loan");

        if loan.repaid || loan.defaulted {
            panic_with_error!(&env, ContractError::NoActiveLoan);
        }

        let cfg = config(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(loan.id), &loan);
        env.storage()
            .persistent()
            .remove(&DataKey::ActiveLoan(borrower.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower.clone()));

        let loan_token = soroban_sdk::token::Client::new(&env, &loan.token_address);
        let mut total_slashed: i128 = 0;
        for v in vouches.iter() {
            if v.token != loan.token_address {
                continue;
            }
            let slash_amount = v.stake * cfg.slash_bps / 10_000;
            let returned = v.stake - slash_amount;
            if returned > 0 {
                loan_token.transfer(&env.current_contract_address(), &v.voucher, &returned);
            }
            total_slashed += slash_amount;
        }

        helpers::add_slash_balance(&env, total_slashed);

        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::DefaultCount(borrower.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::DefaultCount(borrower.clone()), &(count + 1));

        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftExternalClient::new(&env, &nft_addr).burn(&borrower);
        }

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("slashed")),
            (borrower, loan.amount, total_slashed),
        );
    }

    /// Callable by anyone after the loan deadline has passed.
    pub fn auto_slash(env: Env, borrower: Address) {
        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::ActiveLoan(borrower.clone()))
            .and_then(|loan_id: u64| env.storage().persistent().get(&DataKey::Loan(loan_id)))
            .expect("no active loan");

        if loan.repaid || loan.defaulted {
            panic_with_error!(&env, ContractError::NoActiveLoan);
        }
        assert!(
            env.ledger().timestamp() > loan.deadline,
            "loan deadline has not passed"
        );

        let cfg = config(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(loan.id), &loan);
        env.storage()
            .persistent()
            .remove(&DataKey::ActiveLoan(borrower.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower.clone()));

        let loan_token = soroban_sdk::token::Client::new(&env, &loan.token_address);
        let mut total_slash: i128 = 0;
        for v in vouches.iter() {
            if v.token != loan.token_address {
                continue;
            }
            let slash_amount = v.stake * cfg.slash_bps / 10_000;
            let returned = v.stake - slash_amount;
            total_slash += slash_amount;
            if returned > 0 {
                loan_token.transfer(&env.current_contract_address(), &v.voucher, &returned);
            }
        }

        helpers::add_slash_balance(&env, total_slash);

        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::DefaultCount(borrower.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::DefaultCount(borrower.clone()), &(count + 1));

        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftExternalClient::new(&env, &nft_addr).burn(&borrower);
        }

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("autoslash")),
            (borrower, total_slash),
        );
    }

    /// Borrower acknowledges an expired loan; vouchers reclaim their stakes.
    pub fn claim_expired_loan(env: Env, borrower: Address) {
        borrower.require_auth();

        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::ActiveLoan(borrower.clone()))
            .and_then(|loan_id: u64| env.storage().persistent().get(&DataKey::Loan(loan_id)))
            .expect("no active loan");

        if loan.repaid || loan.defaulted {
            panic_with_error!(&env, ContractError::NoActiveLoan);
        }

        let now = env.ledger().timestamp();
        assert!(now >= loan.deadline, "loan has not expired yet");

        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        let loan_token = soroban_sdk::token::Client::new(&env, &loan.token_address);
        for v in vouches.iter() {
            if v.token == loan.token_address {
                loan_token.transfer(&env.current_contract_address(), &v.voucher, &v.stake);
            }
        }

        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(loan.id), &loan);
        env.storage()
            .persistent()
            .remove(&DataKey::ActiveLoan(borrower.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower.clone()));

        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::DefaultCount(borrower.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::DefaultCount(borrower.clone()), &(count + 1));
    }

    /// Admin withdraws accumulated slashed funds.
    pub fn slash_treasury(env: Env, admin_signers: Vec<Address>, recipient: Address) {
        require_admin_approval(&env, &admin_signers);
        let amount: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0);
        assert!(amount > 0, "no slashed funds to withdraw");
        env.storage()
            .instance()
            .set(&DataKey::SlashTreasury, &0i128);
        token_client(&env).transfer(&env.current_contract_address(), &recipient, &amount);
    }

    // ── Voucher cap ───────────────────────────────────────────────────────────

    pub fn set_max_vouchers_per_loan(env: Env, admin_signers: Vec<Address>, max: u32) {
        require_admin_approval(&env, &admin_signers);
        assert!(max > 0, "max_vouchers_per_loan must be greater than zero");
        let mut cfg = config(&env);
        cfg.max_vouchers = max;
        env.storage().instance().set(&DataKey::Config, &cfg);
    }

    pub fn get_max_vouchers_per_loan(env: Env) -> u32 {
        config(&env).max_vouchers
    }

    // ── Loan pool ─────────────────────────────────────────────────────────────

    pub fn create_loan_pool(
        env: Env,
        admin_signers: Vec<Address>,
        borrowers: Vec<Address>,
        amounts: Vec<i128>,
    ) -> Result<u64, ContractError> {
        require_admin_approval(&env, &admin_signers);

        if borrowers.len() != amounts.len() {
            return Err(ContractError::PoolLengthMismatch);
        }
        if borrowers.is_empty() {
            return Err(ContractError::PoolEmpty);
        }

        let cfg = config(&env);
        let primary_token = token_client(&env);
        let mut total_disbursed: i128 = 0;

        for (i, borrower) in borrowers.iter().enumerate() {
            if helpers::has_active_loan(&env, &borrower) {
                return Err(ContractError::PoolBorrowerActiveLoan);
            }
            let amount = amounts.get(i as u32).unwrap();
            total_disbursed += amount;

            let contract_balance =
                primary_token.balance(&env.current_contract_address());
            if contract_balance < amount {
                return Err(ContractError::PoolInsufficientFunds);
            }

            let now = env.ledger().timestamp();
            let deadline = now + cfg.loan_duration;
            let loan_id = helpers::next_loan_id(&env);
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
                    loan_purpose: soroban_sdk::String::from_str(&env, "pool loan"),
                    token_address: cfg.token.clone(),
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

            let lcount: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::LoanCount(borrower.clone()))
                .unwrap_or(0);
            env.storage()
                .persistent()
                .set(&DataKey::LoanCount(borrower.clone()), &(lcount + 1));

            primary_token.transfer(&env.current_contract_address(), &borrower, &amount);
        }

        let pool_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LoanPoolCounter)
            .unwrap_or(0u64)
            .checked_add(1)
            .expect("pool ID overflow");
        env.storage()
            .instance()
            .set(&DataKey::LoanPoolCounter, &pool_id);

        env.storage().persistent().set(
            &DataKey::LoanPool(pool_id),
            &LoanPoolRecord {
                pool_id,
                borrowers: borrowers.clone(),
                amounts,
                created_at: env.ledger().timestamp(),
                total_disbursed,
            },
        );

        // Track all borrowers in the global list.
        let mut blist: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::BorrowerList)
            .unwrap_or(Vec::new(&env));
        for b in borrowers.iter() {
            blist.push_back(b);
        }
        env.storage()
            .persistent()
            .set(&DataKey::BorrowerList, &blist);

        Ok(pool_id)
    }

    pub fn get_loan_pool(env: Env, pool_id: u64) -> Option<LoanPoolRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::LoanPool(pool_id))
    }

    pub fn get_loan_pool_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::LoanPoolCounter)
            .unwrap_or(0)
    }

    pub fn unpause(env: Env, admin_signers: Vec<Address>) {
        admin::unpause(env, admin_signers)
    }

    pub fn get_slash_treasury(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0)
    }

    // ── Vouch delegation ──────────────────────────────────────────────────────

    pub fn vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
        stake: i128,
        token: Address,
    ) -> Result<(), ContractError> {
        vouch::vouch(env, voucher, borrower, stake, token)
    }

    pub fn batch_vouch(
        env: Env,
        voucher: Address,
        borrowers: Vec<Address>,
        stakes: Vec<i128>,
        token: Address,
    ) -> Result<(), ContractError> {
        vouch::batch_vouch(env, voucher, borrowers, stakes, token)
    }

    pub fn increase_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        additional: i128,
    ) -> Result<(), ContractError> {
        vouch::increase_stake(env, voucher, borrower, additional)
    }

    pub fn decrease_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<(), ContractError> {
        vouch::decrease_stake(env, voucher, borrower, amount)
    }

    pub fn withdraw_vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        vouch::withdraw_vouch(env, voucher, borrower)
    }

    pub fn transfer_vouch(
        env: Env,
        from: Address,
        to: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        vouch::transfer_vouch(env, from, to, borrower)
    }

    pub fn vouch_exists(env: Env, voucher: Address, borrower: Address) -> bool {
        vouch::vouch_exists(env, voucher, borrower)
    }

    pub fn voucher_history(env: Env, voucher: Address) -> Vec<Address> {
        vouch::voucher_history(env, voucher)
    }

    pub fn total_vouched(env: Env, borrower: Address) -> Result<i128, ContractError> {
        vouch::total_vouched(env, borrower)
    }

    // ── Loan delegation ───────────────────────────────────────────────────────

    pub fn register_referral(
        env: Env,
        borrower: Address,
        referrer: Address,
    ) -> Result<(), ContractError> {
        loan::register_referral(env, borrower, referrer)
    }

    pub fn get_referrer(env: Env, borrower: Address) -> Option<Address> {
        loan::get_referrer(env, borrower)
    }

    pub fn request_loan(
        env: Env,
        borrower: Address,
        amount: i128,
        threshold: i128,
        loan_purpose: soroban_sdk::String,
        token: Address,
    ) -> Result<(), ContractError> {
        loan::request_loan(env, borrower, amount, threshold, loan_purpose, token)
    }

    pub fn repay(env: Env, borrower: Address, payment: i128) -> Result<(), ContractError> {
        loan::repay(env, borrower, payment)
    }

    pub fn loan_status(env: Env, borrower: Address) -> LoanStatus {
        loan::loan_status(env, borrower)
    }

    pub fn get_loan(env: Env, borrower: Address) -> Option<LoanRecord> {
        loan::get_loan(env, borrower)
    }

    pub fn get_loan_by_id(env: Env, loan_id: u64) -> Option<LoanRecord> {
        loan::get_loan_by_id(env, loan_id)
    }

    pub fn is_eligible(env: Env, borrower: Address, threshold: i128) -> bool {
        loan::is_eligible(env, borrower, threshold)
    }

    pub fn repayment_count(env: Env, borrower: Address) -> u32 {
        loan::repayment_count(env, borrower)
    }

    pub fn loan_count(env: Env, borrower: Address) -> u32 {
        loan::loan_count(env, borrower)
    }

    pub fn default_count(env: Env, borrower: Address) -> u32 {
        loan::default_count(env, borrower)
    }

    // ── Admin delegation ──────────────────────────────────────────────────────

    pub fn add_admin(env: Env, admin_signers: Vec<Address>, new_admin: Address) {
        admin::add_admin(env, admin_signers, new_admin)
    }

    pub fn remove_admin(env: Env, admin_signers: Vec<Address>, admin_to_remove: Address) {
        admin::remove_admin(env, admin_signers, admin_to_remove)
    }

    pub fn rotate_admin(
        env: Env,
        admin_signers: Vec<Address>,
        old_admin: Address,
        new_admin: Address,
    ) {
        admin::rotate_admin(env, admin_signers, old_admin, new_admin)
    }

    pub fn set_admin_threshold(env: Env, admin_signers: Vec<Address>, new_threshold: u32) {
        admin::set_admin_threshold(env, admin_signers, new_threshold)
    }

    pub fn blacklist(env: Env, admin_signers: Vec<Address>, borrower: Address) {
        admin::blacklist(env, admin_signers, borrower)
    }

    pub fn set_config(env: Env, admin_signers: Vec<Address>, cfg: Config) {
        admin::set_config(env, admin_signers, cfg)
    }

    pub fn update_config(
        env: Env,
        admin_signers: Vec<Address>,
        yield_bps: Option<i128>,
        slash_bps: Option<i128>,
    ) {
        admin::update_config(env, admin_signers, yield_bps, slash_bps)
    }

    pub fn set_reputation_nft(env: Env, admin_signers: Vec<Address>, nft_contract: Address) {
        admin::set_reputation_nft(env, admin_signers, nft_contract)
    }

    pub fn set_min_stake(env: Env, admin_signers: Vec<Address>, amount: i128) {
        admin::set_min_stake(env, admin_signers, amount)
    }

    pub fn set_max_loan_amount(env: Env, admin_signers: Vec<Address>, amount: i128) {
        admin::set_max_loan_amount(env, admin_signers, amount)
    }

    pub fn set_min_vouchers(env: Env, admin_signers: Vec<Address>, count: u32) {
        admin::set_min_vouchers(env, admin_signers, count)
    }

    pub fn set_max_loan_to_stake_ratio(env: Env, admin_signers: Vec<Address>, ratio: u32) {
        admin::set_max_loan_to_stake_ratio(env, admin_signers, ratio)
    }

    pub fn set_protocol_fee(env: Env, admin_signers: Vec<Address>, fee_bps: u32) {
        admin::set_protocol_fee(env, admin_signers, fee_bps)
    }

    pub fn set_fee_treasury(env: Env, admin_signers: Vec<Address>, treasury: Address) {
        admin::set_fee_treasury(env, admin_signers, treasury)
    }

    pub fn whitelist_voucher(env: Env, admin_signers: Vec<Address>, voucher: Address) {
        admin::whitelist_voucher(env, admin_signers, voucher)
    }

    pub fn add_allowed_token(env: Env, admin_signers: Vec<Address>, token: Address) {
        admin::add_allowed_token(env, admin_signers, token)
    }

    pub fn remove_allowed_token(env: Env, admin_signers: Vec<Address>, token: Address) {
        admin::remove_allowed_token(env, admin_signers, token)
    }

    pub fn upgrade(env: Env, admin_signers: Vec<Address>, new_wasm_hash: BytesN<32>) {
        admin::upgrade(env, admin_signers, new_wasm_hash)
    }

    pub fn pause(env: Env, admin_signers: Vec<Address>) {
        admin::pause(env, admin_signers)
    }

    pub fn set_referral_bonus_bps(env: Env, admin_signers: Vec<Address>, bonus_bps: u32) {
        require_admin_approval(&env, &admin_signers);
        assert!(bonus_bps <= 10_000, "bonus_bps must not exceed 10000");
        env.storage()
            .instance()
            .set(&DataKey::ReferralBonusBps, &bonus_bps);
    }

    // ── Governance delegation ─────────────────────────────────────────────────

    pub fn vote_slash(
        env: Env,
        voucher: Address,
        borrower: Address,
        approve: bool,
    ) -> Result<(), ContractError> {
        governance::vote_slash(env, voucher, borrower, approve)
    }

    pub fn get_slash_vote(
        env: Env,
        borrower: Address,
    ) -> Option<SlashVoteRecord> {
        governance::get_slash_vote(env, borrower)
    }

    pub fn set_slash_vote_quorum(env: Env, admin_signers: Vec<Address>, quorum_bps: u32) {
        require_admin_approval(&env, &admin_signers);
        governance::set_slash_vote_quorum(&env, quorum_bps);
    }

    pub fn get_slash_vote_quorum(env: Env) -> u32 {
        governance::get_slash_vote_quorum(env)
    }

    // ── Views ─────────────────────────────────────────────────────────────────

    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Config)
    }

    pub fn get_token(env: Env) -> Address {
        config(&env).token
    }

    pub fn get_admins(env: Env) -> Vec<Address> {
        admin::get_admins(env)
    }

    pub fn get_admin_threshold(env: Env) -> u32 {
        admin::get_admin_threshold(env)
    }

    pub fn get_slash_treasury_balance(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0)
    }

    pub fn get_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn get_vouches(env: Env, borrower: Address) -> Option<Vec<VouchRecord>> {
        env.storage().persistent().get(&DataKey::Vouches(borrower))
    }

    pub fn get_contract_balance(env: Env) -> i128 {
        token(&env).balance(&env.current_contract_address())
    }

    pub fn get_reputation(env: Env, borrower: Address) -> u32 {
        let nft_addr: Address = match env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            Some(a) => a,
            None => return 0,
        };
        ReputationNftExternalClient::new(&env, &nft_addr).balance(&borrower)
    }

    pub fn get_protocol_fee(env: Env) -> u32 {
        admin::get_protocol_fee(env)
    }

    pub fn get_fee_treasury(env: Env) -> Option<Address> {
        admin::get_fee_treasury(env)
    }

    pub fn is_blacklisted(env: Env, borrower: Address) -> bool {
        admin::is_blacklisted(env, borrower)
    }

    pub fn get_min_stake(env: Env) -> i128 {
        admin::get_min_stake(env)
    }

    pub fn get_max_loan_amount(env: Env) -> i128 {
        admin::get_max_loan_amount(env)
    }

    pub fn get_min_vouchers(env: Env) -> u32 {
        admin::get_min_vouchers(env)
    }

    pub fn get_max_loan_to_stake_ratio(env: Env) -> u32 {
        admin::get_max_loan_to_stake_ratio(env)
    }

    pub fn get_config(env: Env) -> Config {
        admin::get_config(env)
    }

    pub fn is_whitelisted(env: Env, voucher: Address) -> bool {
        admin::is_whitelisted(env, voucher)
    }

    pub fn get_referral_bonus_bps(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ReferralBonusBps)
            .unwrap_or(DEFAULT_REFERRAL_BONUS_BPS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reputation::ReputationNftContract;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{StellarAssetClient, TokenClient},
        Env, String, Vec,
    };

    // ── Setup helpers ─────────────────────────────────────────────────────────

    fn single_admin_signers(env: &Env, admin: &Address) -> Vec<Address> {
        Vec::from_array(env, [admin.clone()])
    }

    /// Returns (contract_id, token_addr, admin, borrower, voucher)
    fn setup(env: &Env) -> (Address, Address, Address, Address, Address) {
        env.mock_all_auths();

        let deployer = Address::generate(env);
        let admin = Address::generate(env);
        let admins = Vec::from_array(env, [admin.clone()]);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);

        // Fund contract so it can disburse loans and pay yield.
        StellarAssetClient::new(env, &token_id.address()).mint(&contract_id, &10_000_000);

        let client = QuorumCreditContractClient::new(env, &contract_id);
        client.initialize(&deployer, &admins, &1, &token_id.address());

        // Advance time past MIN_VOUCH_AGE (60 s).
        env.ledger().with_mut(|l| l.timestamp = 120);

        let borrower = Address::generate(env);
        let voucher = Address::generate(env);
        StellarAssetClient::new(env, &token_id.address()).mint(&voucher, &10_000_000);

        (contract_id, token_id.address(), admin, borrower, voucher)
    }

    /// Returns (contract_id, token_addr, admin, borrower, voucher, nft_contract_id)
    fn setup_with_reputation(
        env: &Env,
    ) -> (Address, Address, Address, Address, Address, Address) {
        env.mock_all_auths();

        let deployer = Address::generate(env);
        let admin = Address::generate(env);
        let admins = Vec::from_array(env, [admin.clone()]);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);
        let nft_id = env.register_contract(None, ReputationNftContract);

        StellarAssetClient::new(env, &token_id.address()).mint(&contract_id, &10_000_000);

        let client = QuorumCreditContractClient::new(env, &contract_id);
        client.initialize(&deployer, &admins, &1, &token_id.address());

        let nft_client = reputation::ReputationNftContractClient::new(env, &nft_id);
        nft_client.initialize(&contract_id);

        let admin_signers = single_admin_signers(env, &admin);
        client.set_reputation_nft(&admin_signers, &nft_id);

        env.ledger().with_mut(|l| l.timestamp = 120);

        let borrower = Address::generate(env);
        let voucher = Address::generate(env);
        StellarAssetClient::new(env, &token_id.address()).mint(&voucher, &10_000_000);

        (contract_id, token_id.address(), admin, borrower, voucher, nft_id)
    }

    fn purpose(env: &Env) -> String {
        String::from_str(env, "test loan")
    }

    // ── Basic repay / yield tests ─────────────────────────────────────────────

    #[test]
    fn test_repay_gives_voucher_yield() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        let initial_balance = token.balance(&voucher);
        client.vouch(&voucher, &borrower, &1_000_000, &token_addr);
        client.request_loan(&borrower, &100_000, &1_000_000, &purpose(&env), &token_addr);
        // same-day repayment — total_owed = principal + total_yield (no compound interest)
        let loan = client.get_loan(&borrower).unwrap();
        let total_owed = loan.amount + loan.total_yield;
        client.repay(&borrower, &total_owed).unwrap();

        let final_balance = token.balance(&voucher);
        assert!(
            final_balance > initial_balance - 1_000_000,
            "voucher should receive stake + yield"
        );
    }

    #[test]
    fn test_vouch_at_min_yield_stake_earns_nonzero_yield() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000, &token_addr);
        client.request_loan(&borrower, &100_000, &1_000_000, &purpose(&env), &token_addr);

        let loan = client.get_loan(&borrower).unwrap();
        let total_owed = loan.amount + loan.total_yield;
        client.repay(&borrower, &total_owed).unwrap();

        let final_balance = token.balance(&voucher);
        // voucher got back their 1_000_000 stake minus what they put in for the loan,
        // so final balance should exceed initial (10_000_000 - 1_000_000 = 9_000_000).
        assert!(
            final_balance > 9_000_000,
            "voucher yield was zero; got balance {}",
            final_balance
        );
    }

    // ── Reputation NFT tests ──────────────────────────────────────────────────

    #[test]
    fn test_repay_mints_reputation() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher, nft_id) =
            setup_with_reputation(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let nft = reputation::ReputationNftContractClient::new(&env, &nft_id);

        assert_eq!(client.get_reputation(&borrower), 0);

        client.vouch(&voucher, &borrower, &1_000_000, &token_addr);
        client.request_loan(&borrower, &500_000, &1_000_000, &purpose(&env), &token_addr);

        let loan = client.get_loan(&borrower).unwrap();
        let total_owed = loan.amount + loan.total_yield;
        client.repay(&borrower, &total_owed).unwrap();

        assert_eq!(client.get_reputation(&borrower), 1);
        assert_eq!(nft.balance(&borrower), 1);
    }

    #[test]
    fn test_slash_burns_reputation() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher, nft_id) =
            setup_with_reputation(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let nft = reputation::ReputationNftContractClient::new(&env, &nft_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        // First borrower repays — earns 1 rep.
        client.vouch(&voucher, &borrower, &1_000_000, &token_addr);
        client.request_loan(&borrower, &500_000, &1_000_000, &purpose(&env), &token_addr);
        let loan = client.get_loan(&borrower).unwrap();
        client.repay(&borrower, &(loan.amount + loan.total_yield)).unwrap();
        assert_eq!(nft.balance(&borrower), 1);

        // Second borrower gets slashed — rep burns.
        let borrower2 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &2_000_000);

        // Give borrower2 an initial reputation point via the NFT directly.
        nft.mint(&borrower2);
        assert_eq!(nft.balance(&borrower2), 1);

        client.vouch(&voucher2, &borrower2, &1_000_000, &token_addr);
        client.request_loan(&borrower2, &500_000, &1_000_000, &purpose(&env), &token_addr);
        client.slash(&admin_signers, &borrower2);

        assert_eq!(client.get_reputation(&borrower2), 0);
        assert_eq!(nft.balance(&borrower2), 0);
    }

    // ── Loan pool tests ───────────────────────────────────────────────────────

    #[test]
    fn test_create_loan_pool_success() {
        let env = Env::default();
        env.ledger().with_mut(|l| l.timestamp = 1_000_000);
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let borrower1 = Address::generate(&env);
        let borrower2 = Address::generate(&env);
        let voucher1 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher1, &10_000_000);
        token_admin.mint(&voucher2, &10_000_000);
        client.vouch(&voucher1, &borrower1, &2_000_000, &token_addr);
        client.vouch(&voucher2, &borrower2, &2_000_000, &token_addr);

        let borrowers = Vec::from_array(&env, [borrower1.clone(), borrower2.clone()]);
        let amounts = Vec::from_array(&env, [500_000i128, 300_000i128]);

        let pool_id = client.create_loan_pool(&admin_signers, &borrowers, &amounts);
        assert_eq!(pool_id, 1);

        let pool = client.get_loan_pool(&pool_id).unwrap();
        assert_eq!(pool.pool_id, 1);
        assert_eq!(pool.total_disbursed, 800_000);
        assert_eq!(pool.borrowers.len(), 2);

        assert_eq!(client.get_loan(&borrower1).unwrap().amount, 500_000);
        assert_eq!(client.get_loan(&borrower2).unwrap().amount, 300_000);
        assert_eq!(token.balance(&borrower1), 500_000);
        assert_eq!(token.balance(&borrower2), 300_000);
    }

    #[test]
    fn test_create_loan_pool_increments_pool_id() {
        let env = Env::default();
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        assert_eq!(client.get_loan_pool_count(), 0);

        let b1 = Address::generate(&env);
        let v1 = Address::generate(&env);
        token_admin.mint(&v1, &10_000_000);
        client.vouch(&v1, &b1, &2_000_000, &token_addr);
        let bs1 = Vec::from_array(&env, [b1]);
        let am1 = Vec::from_array(&env, [500_000i128]);
        assert_eq!(client.create_loan_pool(&admin_signers, &bs1, &am1), 1);

        let b2 = Address::generate(&env);
        let v2 = Address::generate(&env);
        token_admin.mint(&v2, &10_000_000);
        client.vouch(&v2, &b2, &2_000_000, &token_addr);
        let bs2 = Vec::from_array(&env, [b2]);
        let am2 = Vec::from_array(&env, [500_000i128]);
        assert_eq!(client.create_loan_pool(&admin_signers, &bs2, &am2), 2);

        assert_eq!(client.get_loan_pool_count(), 2);
    }

    #[test]
    fn test_create_loan_pool_length_mismatch_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let borrowers = Vec::from_array(&env, [Address::generate(&env)]);
        let amounts: Vec<i128> = Vec::new(&env);

        let result = client.try_create_loan_pool(&admin_signers, &borrowers, &amounts);
        assert_eq!(result, Err(Ok(ContractError::PoolLengthMismatch)));
    }

    #[test]
    fn test_create_loan_pool_empty_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let borrowers: Vec<Address> = Vec::new(&env);
        let amounts: Vec<i128> = Vec::new(&env);

        let result = client.try_create_loan_pool(&admin_signers, &borrowers, &amounts);
        assert_eq!(result, Err(Ok(ContractError::PoolEmpty)));
    }

    #[test]
    fn test_create_loan_pool_rejects_active_loan_borrower() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &2_000_000, &token_addr);
        client.request_loan(&borrower, &500_000, &2_000_000, &purpose(&env), &token_addr);

        let borrowers = Vec::from_array(&env, [borrower]);
        let amounts = Vec::from_array(&env, [500_000i128]);

        let result = client.try_create_loan_pool(&admin_signers, &borrowers, &amounts);
        assert_eq!(result, Err(Ok(ContractError::PoolBorrowerActiveLoan)));
    }

    #[test]
    fn test_get_loan_pool_unknown_returns_none() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        assert!(client.get_loan_pool(&999u64).is_none());
    }

    // ── Voucher cap tests ─────────────────────────────────────────────────────

    #[test]
    fn test_get_max_vouchers_per_loan_returns_default() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        assert_eq!(client.get_max_vouchers_per_loan(), DEFAULT_MAX_VOUCHERS);
    }

    #[test]
    fn test_set_max_vouchers_per_loan_and_get() {
        let env = Env::default();
        let (contract_id, _, admin, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);
        client.set_max_vouchers_per_loan(&admin_signers, &5);
        assert_eq!(client.get_max_vouchers_per_loan(), 5);
    }

    #[test]
    fn test_vouch_rejected_when_cap_reached() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        client.set_max_vouchers_per_loan(&admin_signers, &2);

        for _ in 0..2 {
            let v = Address::generate(&env);
            token_admin.mint(&v, &1_000_000);
            client.vouch(&v, &borrower, &1_000_000, &token_addr);
        }

        let extra = Address::generate(&env);
        token_admin.mint(&extra, &1_000_000);
        assert!(client.try_vouch(&extra, &borrower, &1_000_000, &token_addr).is_err());
    }

    #[test]
    #[should_panic(expected = "max_vouchers_per_loan must be greater than zero")]
    fn test_set_max_vouchers_per_loan_zero_rejected() {
        let env = Env::default();
        let (contract_id, _, admin, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);
        client.set_max_vouchers_per_loan(&admin_signers, &0);
    }

    // ── Vouch cooldown tests ──────────────────────────────────────────────────

    #[test]
    fn test_vouch_cooldown_blocks_second_vouch_within_window() {
        let env = Env::default();
        env.ledger().with_mut(|l| l.timestamp = 1_000_000);
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.vouch_cooldown_secs = 3_600;
        client.set_config(&admin_signers, &cfg);

        let voucher = Address::generate(&env);
        let borrower1 = Address::generate(&env);
        let borrower2 = Address::generate(&env);
        token_admin.mint(&voucher, &2_000_000);

        client.vouch(&voucher, &borrower1, &1_000_000, &token_addr);
        let result = client.try_vouch(&voucher, &borrower2, &1_000_000, &token_addr);
        assert_eq!(result, Err(Ok(ContractError::VouchCooldownActive)));
    }

    #[test]
    fn test_vouch_cooldown_allows_vouch_after_window_expires() {
        let env = Env::default();
        env.ledger().with_mut(|l| l.timestamp = 1_000_000);
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.vouch_cooldown_secs = 3_600;
        client.set_config(&admin_signers, &cfg);

        let voucher = Address::generate(&env);
        let borrower1 = Address::generate(&env);
        let borrower2 = Address::generate(&env);
        token_admin.mint(&voucher, &2_000_000);

        client.vouch(&voucher, &borrower1, &1_000_000, &token_addr);

        env.ledger().with_mut(|l| l.timestamp += 3_601);
        client.vouch(&voucher, &borrower2, &1_000_000, &token_addr);
        assert!(client.vouch_exists(&voucher, &borrower2));
    }
}
