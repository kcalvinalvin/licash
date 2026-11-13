//! Positive allocation characterization of the unchanged pristine payout.
//! No funding, proof, script execution, fee-quote measurement or sweep occurs.

use anyhow::Result;
use lngap_btc::{
    bitcoin::{Amount, TxOut},
    keys::Seed,
};
use lngap_channel::{ChannelParams, PartyKeys, Role};
use lngap_lamport::winternitz::{WotsParams, WotsSecret};
use lngap_sessions::{
    session::{payout_less, Terms},
    skeleton::Core,
};

fn baseline() -> Terms {
    // The existing regtest contract's monetary terms, not production defaults.
    Terms {
        id: 7,
        unit: Amount::from_sat(1),
        bits: 21,
        low_bits: 9,
        deposit: 500_000,
        t_close: 1_000,
        reserve_alice: Amount::from_sat(20_000),
        reserve_hub: Amount::from_sat(100_000),
    }
}

fn allocation(terms: &Terms, floors: u64, payout_fee: u64) -> Result<Vec<TxOut>> {
    let params = ChannelParams::regtest(Amount::from_sat(20_000_000));
    let pubs = [
        PartyKeys::from_seed(Role::User, Seed::from_label("payout-value/alice")).public(),
        PartyKeys::from_seed(Role::Hub, Seed::from_label("payout-value/hub")).public(),
    ];
    let core = Core {
        terms,
        params: &params,
        pubs: &pubs,
    };
    let keys = WotsSecret::from_entropy(WotsParams::for_bytes(4), [7; 32]).public();
    // Simulate in-contract depletion and separately provision the helper's fee
    // argument. Real holder terminals pass fee zero and add external fee coins;
    // this fixture neither measures nor funds those external recovery costs.
    let available = core.value().to_sat() - floors + payout_fee;
    let outputs = payout_less(
        &core.ctx(),
        terms,
        &keys,
        Amount::from_sat(available),
        Amount::from_sat(payout_fee),
        Amount::from_sat(floors),
    )?;
    assert_eq!(
        outputs.iter().map(|o| o.value.to_sat()).sum::<u64>(),
        available - payout_fee
    );
    Ok(outputs)
}

fn winning_principal(terms: &Terms, outputs: &[TxOut], b: u32) -> u64 {
    (terms.low_bits..terms.bits)
        .zip(outputs)
        .filter(|(bit, _)| b & (1u32 << bit) != 0)
        .map(|(_, output)| output.value.to_sat())
        .sum()
}

#[test]
fn current_bit_outputs_round_principal_down_not_exactly_to_the_claim() -> Result<()> {
    let terms = baseline();
    let outputs = allocation(&terms, 0, 0)?;
    let bits = (terms.bits - terms.low_bits) as usize;
    for (bit, output) in (terms.low_bits..terms.bits).zip(&outputs) {
        assert_eq!(output.value.to_sat(), 1u64 << bit);
    }
    for b in [0, 36, 100, 255, 511, 512, 513, 2_048, (1 << 21) - 1] {
        assert_eq!(
            winning_principal(&terms, &outputs, b),
            u64::from(b / 512) * 512
        );
    }
    assert_eq!(outputs[bits].value.to_sat(), 120_000);
    // The omitted quantum is allocated to the Hub, not made a holder bit.
    assert_eq!(outputs[bits + 1].value.to_sat(), 511);
    Ok(())
}

#[test]
fn remaining_reserve_not_layout_decides_if_rounding_is_covered() -> Result<()> {
    let terms = baseline();
    let reserve_index = (terms.bits - terms.low_bits) as usize;
    for surplus in [0, 510, 511, 512] {
        let floors = terms.reserve_hub.to_sat() - surplus;
        let outputs = allocation(&terms, floors, 1_000)?;
        let reserve = outputs[reserve_index].value.to_sat();
        assert_eq!(reserve, terms.reserve_alice.to_sat() + surplus);
        assert_eq!(outputs[reserve_index + 1].value.to_sat(), 511);
        for b in [0, 36, 100, 255, 511, 512, 2_048, (1 << 21) - 1] {
            let gross_above_own_reserve =
                winning_principal(&terms, &outputs, b) + reserve - terms.reserve_alice.to_sat();
            assert_eq!(
                gross_above_own_reserve >= u64::from(b),
                surplus >= u64::from(b % 512)
            );
        }
    }
    Ok(())
}

#[test]
fn native_budget_checks_actual_post_floor_reserves_without_rounding_debt() -> Result<()> {
    use licash::payout::RecoveryBudget;
    let terms = baseline();
    // Illustrative ceilings only, not measured production fee requirements.
    let budget = RecoveryBudget::new(1, 9, 1_000, 2_000)?;
    let reserve_index = (terms.bits - terms.low_bits) as usize;
    for surplus in [
        budget.minimum_surplus() - 1,
        budget.minimum_surplus(),
        budget.minimum_surplus() + 1,
    ] {
        let outputs = allocation(&terms, terms.reserve_hub.to_sat() - surplus, 0)?;
        let reserve = outputs[reserve_index].value.to_sat();
        assert_eq!(
            budget
                .verify_terminal_reserve(terms.reserve_alice.to_sat(), reserve)
                .is_ok(),
            surplus >= budget.minimum_surplus()
        );
        if surplus < budget.minimum_surplus() {
            continue;
        }
        for b in [0, 36, 100, 255, 511, 512, 2_048, (1 << 21) - 1] {
            let net_above_own_reserve = winning_principal(&terms, &outputs, b) + reserve
                - terms.reserve_alice.to_sat()
                - budget.sweep_fee_max()
                - budget.external_fee_max();
            assert!(net_above_own_reserve >= u64::from(b));
        }
    }
    Ok(())
}
