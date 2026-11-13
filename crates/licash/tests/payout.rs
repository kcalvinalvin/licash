use licash::payout::{RecoveryBudget, UNIT_SATS};

#[test]
fn terminal_budget_includes_rounding_sweep_and_external_recovery_fees() {
    let budget = RecoveryBudget::new(1, 9, 1_000, 2_000).unwrap();
    assert_eq!(UNIT_SATS, 1);
    assert_eq!(budget.low_bits(), 9);
    assert_eq!(budget.sweep_fee_max(), 1_000);
    assert_eq!(budget.external_fee_max(), 2_000);
    assert_eq!(budget.minimum_surplus(), 3_511);
    budget.verify_terminal_reserve(20_000, 23_511).unwrap();
    budget.verify_terminal_reserve(20_000, 23_512).unwrap();
    assert!(budget.verify_terminal_reserve(20_000, 23_510).is_err());
    assert!(budget.verify_terminal_reserve(20_000, 19_999).is_err());
}

#[test]
fn payout_budget_rejects_non_satoshi_units_and_unsupported_bit_counts() {
    for unit in [0, 2, 512, u64::MAX] {
        assert!(RecoveryBudget::new(unit, 9, 0, 0).is_err());
    }
    assert!(RecoveryBudget::new(1, 32, 0, 0).is_err());
    assert!(RecoveryBudget::new(1, u8::MAX, 0, 0).is_err());
}

#[test]
fn allowance_addition_is_checked_without_saturating_or_wrapping() {
    assert!(RecoveryBudget::new(1, 9, u64::MAX, 0).is_err());
    assert!(RecoveryBudget::new(1, 0, u64::MAX, 1).is_err());
    let exact = RecoveryBudget::new(1, 0, u64::MAX - 1, 1).unwrap();
    exact.verify_terminal_reserve(0, u64::MAX).unwrap();
    assert!(exact.verify_terminal_reserve(1, u64::MAX).is_err());
    // Zero omitted bits needs no rounding allowance, but says nothing about
    // dust or relay feasibility (the full graph gate must check those).
    assert_eq!(
        RecoveryBudget::new(1, 0, 0, 0).unwrap().minimum_surplus(),
        0
    );
}

#[test]
fn the_budget_covers_every_rounding_residue_without_changing_owed_value() {
    let budget = RecoveryBudget::new(1, 9, 1_000, 2_000).unwrap();
    let own = 20_000;
    let terminal = own + budget.minimum_surplus();
    budget.verify_terminal_reserve(own, terminal).unwrap();
    for base in [0u32, 512, 1_024, u32::MAX - 511] {
        for remainder in 0..512 {
            let owed = base + remainder;
            let principal = u64::from(owed / 512) * 512;
            let net_after_returning_own_reserve =
                principal + terminal - own - budget.sweep_fee_max() - budget.external_fee_max();
            assert!(net_after_returning_own_reserve >= u64::from(owed));
            if remainder == 511 {
                assert_eq!(net_after_returning_own_reserve, u64::from(owed));
            }
        }
    }
}
