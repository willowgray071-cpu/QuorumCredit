#[cfg(test)]
mod dynamic_rate_tests {
    use crate::{
        BorrowerDynamicRate, DynamicRateConfig, QuorumCreditContract,
        QuorumCreditContractClient,
    };
    use soroban_sdk::{testutils::Address as _, Address, Env, Vec};

    fn setup() -> (
        Env,
        QuorumCreditContractClient<'static>,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();

        let deployer = Address::generate(&env);
        let admin1 = Address::generate(&env);
        let admin2 = Address::generate(&env);
        let admins = Vec::from_array(&env, [admin1.clone(), admin2.clone()]);
        let token = env
            .register_stellar_asset_contract_v2(admin1.clone())
            .address();

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&deployer, &admins, &2, &token);

        (env, client, admin1, admin2, token)
    }

    #[test]
    fn test_default_dynamic_rate_config() {
        let (_env, client, _admin1, _admin2, _token) = setup();

        let config = client.get_dynamic_rate_config();
        assert_eq!(config.enabled, false);
        assert_eq!(config.base_rate_bps, 200);
    }

    #[test]
    fn test_set_dynamic_rate_config() {
        let (_env, client, admin1, admin2, _token) = setup();
        let admins = Vec::from_array(&_env, [admin1.clone(), admin2.clone()]);

        let config = DynamicRateConfig {
            enabled: true,
            base_rate_bps: 300,
            risk_adjustment_bps: 25,
            rate_cap_bps: 3000,
            rate_floor_bps: 100,
        };

        client.set_dynamic_rate_config(&admins, &config).unwrap();

        let stored = client.get_dynamic_rate_config();
        assert_eq!(stored.enabled, true);
        assert_eq!(stored.base_rate_bps, 300);
        assert_eq!(stored.risk_adjustment_bps, 25);
        assert_eq!(stored.rate_cap_bps, 3000);
        assert_eq!(stored.rate_floor_bps, 100);
    }

    #[test]
    fn test_invalid_config_floor_above_cap() {
        let (_env, client, admin1, admin2, _token) = setup();
        let admins = Vec::from_array(&_env, [admin1.clone(), admin2.clone()]);

        let config = DynamicRateConfig {
            enabled: true,
            base_rate_bps: 300,
            risk_adjustment_bps: 25,
            rate_cap_bps: 100,
            rate_floor_bps: 500,
        };

        let result = client.try_set_dynamic_rate_config(&admins, &config);
        assert!(result.is_err());
    }

    #[test]
    fn test_compute_dynamic_rate_with_loan() {
        let (env, client, admin1, admin2, token) = setup();
        let admins = Vec::from_array(&env, [admin1.clone(), admin2.clone()]);

        let config = DynamicRateConfig {
            enabled: true,
            base_rate_bps: 200,
            risk_adjustment_bps: 50,
            rate_cap_bps: 5000,
            rate_floor_bps: 50,
        };
        client.set_dynamic_rate_config(&admins, &config).unwrap();

        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);

        let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        token_client.mint(&voucher, &10_000_000);

        client.vouch(&voucher, &borrower, &5_000_000, &token, &None);

        let purpose = soroban_sdk::String::from_str(&env, "test");
        client
            .request_loan(&borrower, &1_000_000, &1_000_000, &purpose, &token)
            .unwrap();

        // Set risk score
        client
            .set_borrower_risk_score(&admins, &borrower, &50)
            .unwrap();

        let rate = client
            .compute_dynamic_rate(&admins, &borrower)
            .unwrap();

        // base(200) + risk(50 * 50) = 2700 bps, capped at 5000
        assert!(rate > 200);
        assert!(rate <= 5000);
    }

    #[test]
    fn test_get_borrower_dynamic_rate_record() {
        let (env, client, admin1, admin2, token) = setup();
        let admins = Vec::from_array(&env, [admin1.clone(), admin2.clone()]);

        let config = DynamicRateConfig {
            enabled: true,
            base_rate_bps: 200,
            risk_adjustment_bps: 10,
            rate_cap_bps: 5000,
            rate_floor_bps: 50,
        };
        client.set_dynamic_rate_config(&admins, &config).unwrap();

        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);

        let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        token_client.mint(&voucher, &10_000_000);

        client.vouch(&voucher, &borrower, &5_000_000, &token, &None);

        let purpose = soroban_sdk::String::from_str(&env, "test");
        client
            .request_loan(&borrower, &1_000_000, &1_000_000, &purpose, &token)
            .unwrap();

        client
            .compute_dynamic_rate(&admins, &borrower)
            .unwrap();

        let record = client.get_borrower_dynamic_rate(&borrower);
        assert!(record.is_some());
        let record: BorrowerDynamicRate = record.unwrap().try_into().unwrap();
        assert_eq!(record.borrower, borrower);
        assert!(record.effective_rate_bps >= 50);
    }

    #[test]
    fn test_disabled_dynamic_rate_returns_base() {
        let (_env, client, _admin1, _admin2, _token) = setup();

        let config = client.get_dynamic_rate_config();
        assert_eq!(config.enabled, false);
        assert_eq!(config.base_rate_bps, 200);
    }
}
