#[cfg(test)]
mod co_borrower_tests {
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
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

    fn setup_with_loan(
    ) -> (
        Env,
        QuorumCreditContractClient<'static>,
        Address,
        Address,
        Address,
        Address,
    ) {
        let (env, client, admin1, admin2, token) = setup();

        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);

        let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        token_client.mint(&voucher, &10_000_000);

        client.vouch(&voucher, &borrower, &5_000_000, &token, &None);

        let purpose = soroban_sdk::String::from_str(&env, "test loan");
        client
            .request_loan(&borrower, &1_000_000, &1_000_000, &purpose, &token)
            .unwrap();

        (env, client, admin1, token, borrower, voucher)
    }

    #[test]
    fn test_add_co_borrower() {
        let (env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();
        let co_borrower = Address::generate(&env);

        client.add_co_borrower(&borrower, &co_borrower).unwrap();

        let co_borrowers = client.get_co_borrowers(&borrower);
        assert_eq!(co_borrowers.len(), 1);
        assert_eq!(co_borrowers.get(0).unwrap(), co_borrower);
    }

    #[test]
    fn test_add_multiple_co_borrowers() {
        let (env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();
        let co1 = Address::generate(&env);
        let co2 = Address::generate(&env);
        let co3 = Address::generate(&env);

        client.add_co_borrower(&borrower, &co1).unwrap();
        client.add_co_borrower(&borrower, &co2).unwrap();
        client.add_co_borrower(&borrower, &co3).unwrap();

        let co_borrowers = client.get_co_borrowers(&borrower);
        assert_eq!(co_borrowers.len(), 3);
    }

    #[test]
    fn test_remove_co_borrower() {
        let (env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();
        let co1 = Address::generate(&env);
        let co2 = Address::generate(&env);

        client.add_co_borrower(&borrower, &co1).unwrap();
        client.add_co_borrower(&borrower, &co2).unwrap();

        client.remove_co_borrower(&borrower, &co1).unwrap();

        let co_borrowers = client.get_co_borrowers(&borrower);
        assert_eq!(co_borrowers.len(), 1);
        assert_eq!(co_borrowers.get(0).unwrap(), co2);
    }

    #[test]
    fn test_cannot_add_self_as_co_borrower() {
        let (_env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();

        let result = client.try_add_co_borrower(&borrower, &borrower);
        assert!(result.is_err());
    }

    #[test]
    fn test_cannot_add_duplicate_co_borrower() {
        let (env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();
        let co = Address::generate(&env);

        client.add_co_borrower(&borrower, &co).unwrap();
        let result = client.try_add_co_borrower(&borrower, &co);
        assert!(result.is_err());
    }

    #[test]
    fn test_co_borrowers_empty_without_loan() {
        let (env, client, _admin1, _admin2, _token) = setup();
        let borrower = Address::generate(&env);

        let co_borrowers = client.get_co_borrowers(&borrower);
        assert_eq!(co_borrowers.len(), 0);
    }
}
