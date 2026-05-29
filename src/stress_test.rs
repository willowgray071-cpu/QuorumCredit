#[cfg(test)]
mod stress_tests {
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::StellarAssetClient,
        Address, Env, String, Vec,
    };

    struct Setup {
        env: Env,
        client: QuorumCreditContractClient<'static>,
        contract_id: Address,
        token: Address,
        admin: Address,
    }

    fn setup_with_funds(contract_funds: i128) -> Setup {
        let env = Env::default();
        env.mock_all_auths();

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let admins = Vec::from_array(&env, [admin.clone()]);
        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);

        StellarAssetClient::new(&env, &token_id.address()).mint(&contract_id, &contract_funds);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&deployer, &admins, &1, &token_id.address());

        // Advance past MIN_VOUCH_AGE (60s).
        env.ledger().with_mut(|l| l.timestamp = 120);

        Setup { env, client, contract_id, token: token_id.address(), admin }
    }

    fn purpose(env: &Env) -> String {
        String::from_str(env, "stress test")
    }

    fn mint_and_vouch(s: &Setup, voucher: &Address, borrower: &Address, stake: i128) {
        StellarAssetClient::new(&s.env, &s.token).mint(voucher, &stake);
        s.client.vouch(voucher, borrower, &stake, &s.token);
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// 50 independent borrowers each backed by 2 vouchers, all request loans.
    #[test]
    fn test_many_concurrent_borrowers() {
        const N: usize = 50;
        let loan_amount: i128 = 100_000;
        // contract needs: N * (loan_amount + 2% yield) = 50 * 102_000 = 5_100_000
        let s = setup_with_funds(6_000_000);

        let mut borrowers = Vec::new(&s.env);
        for _ in 0..N {
            let borrower = Address::generate(&s.env);
            let v1 = Address::generate(&s.env);
            let v2 = Address::generate(&s.env);
            mint_and_vouch(&s, &v1, &borrower, 300_000);
            mint_and_vouch(&s, &v2, &borrower, 300_000);
            s.client.request_loan(&borrower, &loan_amount, &500_000, &purpose(&s.env), &s.token);
            borrowers.push_back(borrower);
        }

        // Verify all loans are active.
        for borrower in borrowers.iter() {
            assert_eq!(s.client.loan_status(&borrower), crate::LoanStatus::Active);
        }
    }

    /// 50 borrowers each take a loan and repay it; vouchers receive yield.
    #[test]
    fn test_many_sequential_repayments() {
        const N: usize = 50;
        let loan_amount: i128 = 100_000;
        let yield_amount: i128 = 2_000; // 2% of 100_000
        let repayment: i128 = loan_amount + yield_amount;
        // contract needs: N * loan_amount for disbursement + N * yield_amount for yield payouts
        let s = setup_with_funds(10_000_000);

        for _ in 0..N {
            let borrower = Address::generate(&s.env);
            let voucher = Address::generate(&s.env);

            mint_and_vouch(&s, &voucher, &borrower, 600_000);
            s.client.request_loan(&borrower, &loan_amount, &500_000, &purpose(&s.env), &s.token);

            StellarAssetClient::new(&s.env, &s.token).mint(&borrower, &repayment);
            s.client.repay(&borrower, &repayment);

            assert_eq!(s.client.loan_status(&borrower), crate::LoanStatus::Repaid);

            // Voucher earned original stake + 2% yield.
            let voucher_balance = StellarAssetClient::new(&s.env, &s.token).balance(&voucher);
            assert_eq!(voucher_balance, 600_000 + yield_amount);
        }
    }

    /// One borrower backed by many vouchers (up to max_vouchers limit).
    #[test]
    fn test_single_borrower_many_vouchers() {
        // DEFAULT_MAX_VOUCHERS = 100; use 99 to stay under the cap.
        const N: usize = 99;
        let stake_per_voucher: i128 = 10_000;
        let s = setup_with_funds(1_000_000);

        let borrower = Address::generate(&s.env);
        for _ in 0..N {
            let voucher = Address::generate(&s.env);
            mint_and_vouch(&s, &voucher, &borrower, stake_per_voucher);
        }

        let vouches = s.client.get_vouches(&borrower).unwrap_or(Vec::new(&s.env));
        assert_eq!(vouches.len() as usize, N);

        let total_stake: i128 = vouches.iter().map(|v| v.stake).fold(0i128, |acc, s| acc + s);
        assert_eq!(total_stake, stake_per_voucher * N as i128);
    }

    /// High-volume slash scenario: 20 borrowers default, treasury accumulates slashed funds.
    #[test]
    fn test_many_slashes_accumulate_treasury() {
        const N: usize = 20;
        let stake: i128 = 200_000;
        let loan_amount: i128 = 100_000;
        let s = setup_with_funds(5_000_000);

        let admins = Vec::from_array(&s.env, [s.admin.clone()]);

        for _ in 0..N {
            let borrower = Address::generate(&s.env);
            let voucher = Address::generate(&s.env);
            mint_and_vouch(&s, &voucher, &borrower, stake);
            s.client.request_loan(&borrower, &loan_amount, &100_000, &purpose(&s.env), &s.token);
            s.client.slash(&admins, &borrower);
        }

        // Each slash burns 50% of stake (200_000 * 50% = 100_000) × 20 = 2_000_000.
        assert_eq!(s.client.get_slash_treasury_balance(), 100_000 * N as i128);
    }

    /// Mixed workload: concurrent vouches, loans, repayments, and slashes.
    #[test]
    fn test_mixed_high_volume_workload() {
        const REPAY_COUNT: usize = 15;
        const SLASH_COUNT: usize = 10;
        let loan_amount: i128 = 100_000;
        let repayment: i128 = 102_000;
        // contract needs: (REPAY_COUNT + SLASH_COUNT) * loan_amount + REPAY_COUNT * 2_000 yield
        let s = setup_with_funds(5_000_000);

        let admins = Vec::from_array(&s.env, [s.admin.clone()]);

        // Repaid loans.
        for _ in 0..REPAY_COUNT {
            let borrower = Address::generate(&s.env);
            let voucher = Address::generate(&s.env);
            mint_and_vouch(&s, &voucher, &borrower, 600_000);
            s.client.request_loan(&borrower, &loan_amount, &500_000, &purpose(&s.env), &s.token);
            StellarAssetClient::new(&s.env, &s.token).mint(&borrower, &repayment);
            s.client.repay(&borrower, &repayment);
            assert_eq!(s.client.loan_status(&borrower), crate::LoanStatus::Repaid);
        }

        // Defaulted loans.
        for _ in 0..SLASH_COUNT {
            let borrower = Address::generate(&s.env);
            let voucher = Address::generate(&s.env);
            mint_and_vouch(&s, &voucher, &borrower, 600_000);
            s.client.request_loan(&borrower, &loan_amount, &500_000, &purpose(&s.env), &s.token);
            s.client.slash(&admins, &borrower);
            assert_eq!(s.client.loan_status(&borrower), crate::LoanStatus::Defaulted);
        }
    }

    /// Batch vouch: one voucher backs many borrowers in a single call.
    #[test]
    fn test_batch_vouch_high_volume() {
        const N: u32 = 30;
        let stake_per: i128 = 100_000;
        let s = setup_with_funds(1_000_000);

        let voucher = Address::generate(&s.env);
        let total_stake = stake_per * N as i128;
        StellarAssetClient::new(&s.env, &s.token).mint(&voucher, &total_stake);

        let mut borrowers = Vec::new(&s.env);
        let mut stakes = Vec::new(&s.env);
        for _ in 0..N {
            borrowers.push_back(Address::generate(&s.env));
            stakes.push_back(stake_per);
        }

        s.client.batch_vouch(&voucher, &borrowers, &stakes, &s.token);

        // Verify each borrower has a vouch from this voucher.
        for borrower in borrowers.iter() {
            let vouches = s.client.get_vouches(&borrower).unwrap_or(Vec::new(&s.env));
            assert_eq!(vouches.len(), 1);
            assert_eq!(vouches.get(0).unwrap().voucher, voucher);
        }
    }
}
