//! Positive Core collection from synthetic pristine payout outputs. This is
//! not final-D, publication, guest/proof or complete dispute-graph acceptance.

use anyhow::{ensure, Context, Result};
use bitcoincore_rpc::{json::FundRawTransactionOptions, RpcApi};
use licash::payout::RecoveryBudget;
use lngap_btc::{
    bitcoin::{absolute, transaction, Amount, OutPoint, Transaction, TxIn, TxOut},
    keys::Seed,
    regtest::Regtest,
    sighash::sign_tapscript,
    taptree::TapTree,
    tx::build_tx,
    witness::tapscript_witness,
};
use lngap_channel::{ChannelParams, PartyKeys, Role};
use lngap_lamport::winternitz::{WotsParams, WotsSecret};
use lngap_sessions::{
    session::{bit_tree, payout_less, reserve_tree, Terms},
    skeleton::Core,
};

#[test]
fn winning_bits_and_delayed_reserve_collect_to_the_holder_with_measured_fees() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let alice = PartyKeys::from_seed(Role::User, Seed::from_label("native-payout/alice"));
    let hub = PartyKeys::from_seed(Role::Hub, Seed::from_label("native-payout/hub"));
    let pubs = [alice.public(), hub.public()];
    let params = ChannelParams::regtest(Amount::from_sat(20_000_000));
    let terms = Terms {
        id: 7,
        unit: Amount::from_sat(1),
        bits: 21,
        low_bits: 9,
        deposit: 500_000,
        t_close: 1_000,
        reserve_alice: Amount::from_sat(20_000),
        reserve_hub: Amount::from_sat(100_000),
    };
    let core = Core {
        terms: &terms,
        params: &params,
        pubs: &pubs,
    };
    let ctx = core.ctx();
    // Illustrative ceilings, not production measurements or an opening quote.
    let budget = RecoveryBudget::new(1, 9, 20_000, 30_000)?;
    let floors = terms.reserve_hub.to_sat() - budget.minimum_surplus();
    let reserve_index = (terms.bits - terms.low_bits) as usize;

    for (case, b) in [0u32, 36, 511, 512, 2_048, (1 << 21) - 1]
        .into_iter()
        .enumerate()
    {
        // Distinct one-time key context for each independently funded fixture.
        let signer = WotsSecret::from_entropy(WotsParams::for_bytes(4), [case as u8 + 7; 32]);
        let b_key = signer.public();
        let reveal: Vec<_> = signer
            .sign(&b.to_be_bytes())?
            .consumption_order()
            .into_iter()
            .rev()
            .collect();
        let outputs = payout_less(
            &ctx,
            &terms,
            &b_key,
            Amount::from_sat(core.value().to_sat() - floors),
            Amount::ZERO,
            Amount::from_sat(floors),
        )?;
        let reserve = reserve_tree(&ctx, &terms, &b_key)?;
        ensure!(outputs[reserve_index].script_pubkey == reserve.script_pubkey());
        budget.verify_terminal_reserve(
            terms.reserve_alice.to_sat(),
            outputs[reserve_index].value.to_sat(),
        )?;

        // Core funds the exact payout outputs directly, not via BABE. Its fee
        // is the only external cost in this isolated terminal-collection test.
        let coin = rt
            .rpc
            .list_unspent(Some(1), None, None, None, None)?
            .into_iter()
            .next()
            .context("confirmed synthetic-payout funding coin")?;
        let unsigned = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: coin.txid,
                    vout: coin.vout,
                },
                ..Default::default()
            }],
            output: outputs.clone(),
        };
        let funded = rt
            .rpc
            .fund_raw_transaction(
                &unsigned,
                Some(&FundRawTransactionOptions {
                    change_position: Some(outputs.len() as u32),
                    ..Default::default()
                }),
                None,
            )
            .context("fund synthetic payout")?;
        let external_fee = funded.fee.to_sat();
        ensure!(external_fee <= budget.external_fee_max());
        let signed = rt
            .rpc
            .sign_raw_transaction_with_wallet(&funded.hex, None, None)
            .context("sign synthetic payout")?;
        ensure!(signed.complete);
        let payout = signed.transaction()?;
        ensure!(payout.output[..outputs.len()] == outputs);
        let (payout_id, _) = rt.send_and_confirm(&payout)?;

        let mut inputs: Vec<(OutPoint, TxOut, TapTree, String, bool)> = Vec::new();
        for bit in terms.low_bits..terms.bits {
            if b & (1 << bit) == 0 {
                continue;
            }
            let tree = bit_tree(&ctx, bit, &b_key)?;
            let index = (bit - terms.low_bits) as usize;
            ensure!(payout.output[index].script_pubkey == tree.script_pubkey());
            inputs.push((
                OutPoint {
                    txid: payout_id,
                    vout: index as u32,
                },
                payout.output[index].clone(),
                tree,
                format!("alice_bit_{bit}"),
                true,
            ));
        }
        let principal: u64 = inputs
            .iter()
            .map(|(_, output, _, _, _)| output.value.to_sat())
            .sum();
        ensure!(principal == u64::from(b / 512) * 512);
        inputs.push((
            OutPoint {
                txid: payout_id,
                vout: reserve_index as u32,
            },
            payout.output[reserve_index].clone(),
            reserve,
            "alice_reserve".into(),
            false,
        ));
        let prevouts: Vec<_> = inputs
            .iter()
            .map(|(_, output, _, _, _)| output.clone())
            .collect();
        let gross: u64 = prevouts.iter().map(|o| o.value.to_sat()).sum();
        let build = |fee: u64| -> Result<Transaction> {
            let sequences = inputs
                .iter()
                .map(|(op, _, tree, name, _)| Ok((*op, tree.leaf(name)?.timelock.sequence())))
                .collect::<Result<Vec<_>>>()?;
            let mut tx = build_tx(
                &sequences,
                vec![TxOut {
                    value: Amount::from_sat(
                        gross
                            .checked_sub(fee)
                            .context("collection fee exceeds outputs")?,
                    ),
                    script_pubkey: pubs[0].payout_spk.clone(),
                }],
                absolute::LockTime::ZERO,
            );
            for (n, (_, _, tree, name, bit)) in inputs.iter().enumerate() {
                let leaf = tree.leaf(name)?;
                let mut args = if *bit { reveal.clone() } else { vec![] };
                args.push(
                    sign_tapscript(&alice.payment, &tx, n, &prevouts, &leaf.script)?
                        .as_ref()
                        .to_vec(),
                );
                tx.input[n].witness =
                    tapscript_witness(&args, &leaf.script, &tree.control_block(name)?);
            }
            Ok(tx)
        };
        let vsize = build(0)?.vsize() as u64;
        let sweep_fee = 2 * (vsize + 10);
        ensure!(sweep_fee <= budget.sweep_fee_max());
        let sweep = build(sweep_fee)?;
        ensure!(sweep.vsize() as u64 == vsize);
        rt.mine(u64::from(params.delta))?;
        let (sweep_id, _) = rt.send_and_confirm(&sweep)?;
        let collected = rt.get_tx(&sweep_id)?;
        ensure!(
            collected.output.len() == 1 && collected.output[0].script_pubkey == pubs[0].payout_spk
        );
        let received = collected.output[0].value.to_sat();
        ensure!(gross - received == sweep_fee);
        let net_above_own_reserve = received
            .checked_sub(terms.reserve_alice.to_sat())
            .and_then(|value| value.checked_sub(external_fee))
            .context("recovery cannot return holder reserve and fees")?;
        ensure!(net_above_own_reserve >= u64::from(b), "case b={b}");
        println!("b={b}, principal={principal}, inputs={}, sweep_vb={vsize}, C={sweep_fee}, X={external_fee}, net_above_own_reserve={net_above_own_reserve}", inputs.len());
    }
    Ok(())
}
