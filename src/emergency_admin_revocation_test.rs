/// Tests for the emergency admin revocation mechanism.
///
/// Issue: If an admin key is compromised, the protocol is exposed until a new
/// admin is manually set. This emergency revocation mechanism allows the remaining
/// admins to remove a compromised key by requiring N-1 of N admins to approve.
#[cfg(test)]
mod emergency_admin_revocation_tests {
    use crate::{
        errors::ContractError,
        helpers::require_admin_approval,
        types::{Config, DataKey, DEFAULT_YIELD_BPS, DEFAULT_SLASH_BPS, DEFAULT_MAX_VOUCHERS,
                DEFAULT_MIN_LOAN_AMOUNT, DEFAULT_LOAN_DURATION, DEFAULT_MAX_LOAN_TO_STAKE_RATIO,
                DEFAULT_MIN_VOUCH_AGE_SECS, DEFAULT_LIQUIDITY_MINING_RATE_BPS,
                DEFAULT_VOTING_PERIOD_SECONDS, DEFAULT_DYNAMIC_SLASH_THRESHOLD,
                DEFAULT_LOAN_SIZE_SLASH_ENABLED, DEFAULT_LOAN_SIZE_SLASH_MAX_BPS,
                DEFAULT_CONFIRMATION_REQUIRED, RedistributionRule, RateLimitConfig,
                DEFAULT_RATE_LIMIT_COUNT, DEFAULT_RATE_LIMIT_WINDOW_SECS},
    };
    use soroban_sdk::{
        testutils::Address as _,
        Address, Env, String, Vec,
    };

    /// Helper: create a fresh Env and store a Config with the given admins
    fn setup_env_with_admins(admins_list: &[Address], threshold: u32) -> (Env, ()) {
        let env = Env::default();
        env.mock_all_auths();

        let token = Address::generate(&env);
        let mut admins = Vec::new(&env);
        for a in admins_list {
            admins.push_back(a.clone());
        }

        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admins,
                admin_threshold: threshold,
                admin_whitelist: Vec::new(&env),
                admin_blacklist: Vec::new(&env),
                token,
                allowed_tokens: Vec::new(&env),
                yield_bps: DEFAULT_YIELD_BPS,
                slash_bps: DEFAULT_SLASH_BPS,
                max_vouchers: DEFAULT_MAX_VOUCHERS,
                min_loan_amount: DEFAULT_MIN_LOAN_AMOUNT,
                loan_duration: DEFAULT_LOAN_DURATION,
                max_loan_to_stake_ratio: DEFAULT_MAX_LOAN_TO_STAKE_RATIO,
                grace_period: 0,
                min_vouch_age_secs: DEFAULT_MIN_VOUCH_AGE_SECS,
                prepayment_penalty_bps: 0,
                liquidity_mining_rate_bps: DEFAULT_LIQUIDITY_MINING_RATE_BPS,
                voting_period_seconds: DEFAULT_VOTING_PERIOD_SECONDS,
                slash_cooldown_seconds: 0,
                emergency_pause_enabled: false,
                early_repayment_discount_bps: 0,
                oracle_address: None,
                slash_delay_seconds: 0,
                successor_admin: None,
                rate_limit_config: RateLimitConfig {
                    window_secs: DEFAULT_RATE_LIMIT_WINDOW_SECS,
                    max_calls: DEFAULT_RATE_LIMIT_COUNT,
                    enabled: false,
                },
                dynamic_slash_threshold: DEFAULT_DYNAMIC_SLASH_THRESHOLD,
                loan_size_slash_enabled: DEFAULT_LOAN_SIZE_SLASH_ENABLED,
                loan_size_slash_max_bps: DEFAULT_LOAN_SIZE_SLASH_MAX_BPS,
                recovery_percentage: 0,
                admin_compensation_bps: 0,
                removal_vote_threshold: 0,
                confirmation_required: DEFAULT_CONFIRMATION_REQUIRED,
                redistribution_rule: RedistributionRule::Treasury,
                immunity_period_seconds: 0,
                insurance_premium_bps: 0,
            },
        );

        (env, ())
    }

    /// revoke_admin succeeds: 3-of-3 admins, N-1 = 2 signers revoke the third.
    /// After revocation:
    ///   - target is no longer in Config.admins
    ///   - DataKey::RevokedAdmin(target) = true
    ///   - threshold is adjusted down if necessary
    #[test]
    fn test_revoke_admin_success_three_admins() {
        let env = Env::default();
        env.mock_all_auths();

        let admin_a = Address::generate(&env);
        let admin_b = Address::generate(&env);
        let compromised = Address::generate(&env);
        let admins_list = [admin_a.clone(), admin_b.clone(), compromised.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 2);

        let mut signers = Vec::new(&env);
        signers.push_back(admin_a.clone());
        signers.push_back(admin_b.clone());

        let reason = String::from_str(&env, "Private key suspected compromised");

        let result = crate::admin::revoke_admin(
            env.clone(),
            signers,
            compromised.clone(),
            reason,
        );

        assert_eq!(result, Ok(()));

        // Target removed from Config.admins
        let cfg: Config = env.storage().instance().get(&DataKey::Config).unwrap();
        assert!(
            !cfg.admins.iter().any(|a| a == compromised),
            "compromised admin should be removed from admins list"
        );
        assert_eq!(cfg.admins.len(), 2, "two admins should remain");

        // RevokedAdmin flag persisted
        let is_revoked: bool = env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::RevokedAdmin(compromised.clone()))
            .unwrap_or(false);
        assert!(is_revoked, "RevokedAdmin flag should be set");
    }

    /// revoke_admin succeeds with a 2-admin setup: only 1 signer required (N-1 = 1).
    #[test]
    fn test_revoke_admin_success_two_admins() {
        let env = Env::default();
        env.mock_all_auths();

        let admin_a = Address::generate(&env);
        let compromised = Address::generate(&env);
        let admins_list = [admin_a.clone(), compromised.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 1);

        let mut signers = Vec::new(&env);
        signers.push_back(admin_a.clone());

        let reason = String::from_str(&env, "Phishing attack detected");

        let result = crate::admin::revoke_admin(
            env.clone(),
            signers,
            compromised.clone(),
            reason,
        );

        assert_eq!(result, Ok(()));

        let cfg: Config = env.storage().instance().get(&DataKey::Config).unwrap();
        assert_eq!(cfg.admins.len(), 1);
        assert!(!cfg.admins.iter().any(|a| a == compromised));
    }

    /// revoke_admin with only 1 admin returns InvalidAdminThreshold —
    /// cannot leave zero admins.
    #[test]
    fn test_revoke_admin_fails_when_only_one_admin() {
        let env = Env::default();
        env.mock_all_auths();

        let sole_admin = Address::generate(&env);
        let admins_list = [sole_admin.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 1);

        let signers: Vec<Address> = Vec::new(&env);
        let reason = String::from_str(&env, "Irrelevant");

        let result = crate::admin::revoke_admin(
            env.clone(),
            signers,
            sole_admin.clone(),
            reason,
        );

        assert_eq!(result, Err(ContractError::InvalidAdminThreshold));
    }

    /// revoke_admin returns AdminNotFound when target is not a registered admin.
    #[test]
    fn test_revoke_admin_fails_when_target_not_admin() {
        let env = Env::default();
        env.mock_all_auths();

        let admin_a = Address::generate(&env);
        let admin_b = Address::generate(&env);
        let stranger = Address::generate(&env);
        let admins_list = [admin_a.clone(), admin_b.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 1);

        let mut signers = Vec::new(&env);
        signers.push_back(admin_a.clone());

        let reason = String::from_str(&env, "Not an admin anyway");

        let result = crate::admin::revoke_admin(
            env.clone(),
            signers,
            stranger.clone(),
            reason,
        );

        assert_eq!(result, Err(ContractError::AdminNotFound));
    }

    /// revoke_admin returns AdminAlreadyRevoked when called twice on the same target.
    #[test]
    fn test_revoke_admin_fails_when_already_revoked() {
        let env = Env::default();
        env.mock_all_auths();

        let admin_a = Address::generate(&env);
        let admin_b = Address::generate(&env);
        let compromised = Address::generate(&env);
        let admins_list = [admin_a.clone(), admin_b.clone(), compromised.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 2);

        let mut signers = Vec::new(&env);
        signers.push_back(admin_a.clone());
        signers.push_back(admin_b.clone());

        let reason = String::from_str(&env, "Key leaked");

        // First revocation succeeds
        crate::admin::revoke_admin(
            env.clone(),
            signers.clone(),
            compromised.clone(),
            reason.clone(),
        )
        .expect("first revocation should succeed");

        // Re-add compromised to Config.admins to test the double-revocation path
        // (in practice they're removed, but we want to test the revoked flag check)
        let mut cfg: Config = env.storage().instance().get(&DataKey::Config).unwrap();
        cfg.admins.push_back(compromised.clone());
        env.storage().instance().set(&DataKey::Config, &cfg);

        // Second revocation must fail with AdminAlreadyRevoked
        let result = crate::admin::revoke_admin(
            env.clone(),
            signers,
            compromised.clone(),
            reason,
        );

        assert_eq!(result, Err(ContractError::AdminAlreadyRevoked));
    }

    /// revoke_admin returns UnauthorizedCaller when fewer than N-1 signers are provided.
    #[test]
    fn test_revoke_admin_fails_when_insufficient_signers() {
        let env = Env::default();
        env.mock_all_auths();

        let admin_a = Address::generate(&env);
        let admin_b = Address::generate(&env);
        let compromised = Address::generate(&env);
        // 3 admins → need 2 signers; provide only 1
        let admins_list = [admin_a.clone(), admin_b.clone(), compromised.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 2);

        let mut signers = Vec::new(&env);
        signers.push_back(admin_a.clone()); // only 1, needs 2

        let reason = String::from_str(&env, "Key compromised");

        let result = crate::admin::revoke_admin(
            env.clone(),
            signers,
            compromised.clone(),
            reason,
        );

        assert_eq!(result, Err(ContractError::UnauthorizedCaller));
    }

    /// revoked admin cannot participate in require_admin_approval.
    #[test]
    fn test_revoked_admin_excluded_from_approval() {
        let env = Env::default();
        env.mock_all_auths();

        let admin_a = Address::generate(&env);
        let compromised = Address::generate(&env);
        let admins_list = [admin_a.clone(), compromised.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 1);

        // Mark compromised as revoked in persistent storage
        env.storage()
            .persistent()
            .set(&DataKey::RevokedAdmin(compromised.clone()), &true);

        // Also remove from Config.admins to reflect the post-revocation state
        let mut cfg: Config = env.storage().instance().get(&DataKey::Config).unwrap();
        cfg.admins = {
            let mut v = Vec::new(&env);
            v.push_back(admin_a.clone());
            v
        };
        env.storage().instance().set(&DataKey::Config, &cfg);

        // Attempting to use the revoked admin as an approver must panic
        let mut bad_signers = Vec::new(&env);
        bad_signers.push_back(compromised.clone());

        let result = std::panic::catch_unwind(|| {
            require_admin_approval(&env, &bad_signers);
        });
        assert!(
            result.is_err(),
            "require_admin_approval should panic when a revoked admin is used"
        );
    }

    /// After revocation, is_admin_revoked returns true for the revoked address.
    #[test]
    fn test_is_admin_revoked_returns_true_after_revocation() {
        let env = Env::default();
        env.mock_all_auths();

        let admin_a = Address::generate(&env);
        let compromised = Address::generate(&env);
        let admins_list = [admin_a.clone(), compromised.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 1);

        let mut signers = Vec::new(&env);
        signers.push_back(admin_a.clone());

        crate::admin::revoke_admin(
            env.clone(),
            signers,
            compromised.clone(),
            String::from_str(&env, "Security breach"),
        )
        .expect("revocation should succeed");

        assert!(
            crate::admin::is_admin_revoked(env.clone(), compromised.clone()),
            "is_admin_revoked should return true"
        );
        assert!(
            !crate::admin::is_admin_revoked(env.clone(), admin_a.clone()),
            "is_admin_revoked should return false for non-revoked admin"
        );
    }

    /// The target admin cannot include themselves in the signing list.
    #[test]
    fn test_revoke_admin_target_cannot_sign_own_revocation() {
        let env = Env::default();
        env.mock_all_auths();

        let admin_a = Address::generate(&env);
        let compromised = Address::generate(&env);
        let admins_list = [admin_a.clone(), compromised.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 1);

        // Provide the compromised admin as one of the signers
        let mut signers = Vec::new(&env);
        signers.push_back(compromised.clone());

        let result = crate::admin::revoke_admin(
            env.clone(),
            signers,
            compromised.clone(),
            String::from_str(&env, "Self-revoke attempt"),
        );

        assert_eq!(result, Err(ContractError::UnauthorizedCaller));
    }

    /// admin_threshold is reduced if it would exceed remaining admin count after revocation.
    #[test]
    fn test_revoke_admin_adjusts_threshold_when_necessary() {
        let env = Env::default();
        env.mock_all_auths();

        let admin_a = Address::generate(&env);
        let admin_b = Address::generate(&env);
        let compromised = Address::generate(&env);
        // threshold = 3 (all must sign normally); after revocation 2 remain — threshold must drop
        let admins_list = [admin_a.clone(), admin_b.clone(), compromised.clone()];
        let (env, _) = setup_env_with_admins(&admins_list, 3);

        let mut signers = Vec::new(&env);
        signers.push_back(admin_a.clone());
        signers.push_back(admin_b.clone());

        crate::admin::revoke_admin(
            env.clone(),
            signers,
            compromised.clone(),
            String::from_str(&env, "Key stolen"),
        )
        .expect("revocation should succeed");

        let cfg: Config = env.storage().instance().get(&DataKey::Config).unwrap();
        assert!(
            cfg.admin_threshold <= cfg.admins.len(),
            "threshold must not exceed remaining admin count; got threshold={} admins={}",
            cfg.admin_threshold,
            cfg.admins.len()
        );
    }
}
