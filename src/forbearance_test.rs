#[cfg(test)]
mod forbearance_tests {
    use crate::{
        ForbearanceRecord, ForbearanceStatus, QuorumCreditContract,
        QuorumCreditContractClient,
    };
    use soroban_sdk::{testutils::Address as _, Address, Env, Vec};

    fn setup_with_loan() -> (
        Env,
        QuorumCreditContractClient<'static>,
        Address,
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
    fn test_request_forbearance() {
        let (_env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();

        client.request_forbearance(&borrower, &None).unwrap();

        let loan = client.get_loan(&borrower).unwrap();
        let forbearance = client.get_forbearance(&loan.id);
        assert!(forbearance.is_some());
        let fb: ForbearanceRecord = forbearance.unwrap().try_into().unwrap();
        assert_eq!(fb.status, ForbearanceStatus::Active);
        assert_eq!(fb.period_number, 1);
    }

    #[test]
    fn test_forbearance_extends_deadline() {
        let (_env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();

        let loan_before = client.get_loan(&borrower).unwrap();
        let original_deadline = loan_before.deadline;

        let custom_duration: u64 = 14 * 24 * 60 * 60; // 14 days
        client
            .request_forbearance(&borrower, &Some(custom_duration))
            .unwrap();

        let loan_after = client.get_loan(&borrower).unwrap();
        assert_eq!(loan_after.deadline, original_deadline + custom_duration);
    }

    #[test]
    fn test_end_forbearance() {
        let (_env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();

        client.request_forbearance(&borrower, &None).unwrap();
        client.end_forbearance(&borrower).unwrap();

        let loan = client.get_loan(&borrower).unwrap();
        let fb: ForbearanceRecord = client
            .get_forbearance(&loan.id)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(fb.status, ForbearanceStatus::Ended);
    }

    #[test]
    fn test_cannot_double_forbear() {
        let (_env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();

        client.request_forbearance(&borrower, &None).unwrap();

        let result = client.try_request_forbearance(&borrower, &None);
        assert!(result.is_err());
    }

    #[test]
    fn test_max_forbearance_periods() {
        let (_env, client, _admin1, _token, borrower, _voucher) = setup_with_loan();

        // First forbearance
        client.request_forbearance(&borrower, &None).unwrap();
        client.end_forbearance(&borrower).unwrap();

        // Second forbearance
        client.request_forbearance(&borrower, &None).unwrap();
        client.end_forbearance(&borrower).unwrap();

        // Third should fail (MAX_FORBEARANCE_PERIODS = 2)
        let result = client.try_request_forbearance(&borrower, &None);
        assert!(result.is_err());
    }

    #[test]
    fn test_forbearance_not_found() {
        let (_env, client, _admin1, _token, _borrower, _voucher) = setup_with_loan();

        let result = client.get_forbearance(&999);
        assert!(result.is_none());
    }
}
