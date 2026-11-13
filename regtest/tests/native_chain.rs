//! Real Core 30 header, activation and BIP141 inclusion for the native layer.
//! This does not activate a v2 Wallet, recovery contract or guest statement.

use anyhow::{ensure, Context, Result};
use bitcoincore_rpc::{json::FundRawTransactionOptions, RpcApi};
use licash::{
    chain::{self, Activation, ActivationRef, Chain, Checkpoint, Inclusion, WitnessInclusion},
    spend::{ChainProfile, TimedTerms},
};
use lngap_btc::{
    bitcoin::{
        absolute,
        consensus::serialize,
        hashes::{sha256, Hash},
        transaction, Amount, OutPoint, ScriptBuf, Transaction, TxIn, TxOut,
    },
    regtest::Regtest,
};

#[test]
fn native_anchored_headers_activation_and_witness_commitments_match_core30() -> Result<()> {
    let rt = Regtest::start_standard().context("start isolated Core 30 node")?;
    let anchor_hash = rt.rpc.get_best_block_hash().context("read anchor tip")?;
    let anchor = rt.rpc.get_block_header(&anchor_hash)?;
    let height = u32::try_from(rt.rpc.get_block_header_info(&anchor_hash)?.height)?;
    let mut times = vec![anchor.time];
    let mut previous = anchor;
    for _ in 1..11 {
        previous = rt.rpc.get_block_header(&previous.prev_blockhash)?;
        times.push(previous.time);
    }
    times.reverse();
    let checkpoint = Checkpoint::new(
        ChainProfile::Core30DefaultRegtest,
        height,
        serialize(&anchor).try_into().expect("80-byte header"),
        times,
        None,
    )?;
    let secret = [83; 32];
    let coin = rt
        .rpc
        .list_unspent(Some(1), None, None, None, None)?
        .into_iter()
        .next()
        .context("activation fee input")?;
    let skeleton = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: coin.txid,
                vout: coin.vout,
            },
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(chain::activation_script(&secret)),
        }],
    };
    let funded = rt
        .rpc
        .fund_raw_transaction(
            &skeleton,
            Some(&FundRawTransactionOptions {
                change_position: Some(0),
                ..Default::default()
            }),
            None,
        )?
        .transaction()?;
    let signed = rt
        .rpc
        .sign_raw_transaction_with_wallet(&funded, None, None)?;
    ensure!(signed.complete, "activation transaction signing");
    let signed = signed.transaction()?;
    let activation_output = u32::try_from(
        signed
            .output
            .iter()
            .position(|output| output.script_pubkey.as_bytes() == chain::activation_script(&secret))
            .context("funded activation output")?,
    )?;
    ensure!(
        activation_output == 1,
        "change deliberately precedes the activation output"
    );
    let (txid, _) = rt
        .send_and_confirm(&signed)
        .context("broadcast and confirm activation")?;
    let publication_height = rt.height()?;
    let publication_hash = rt.rpc.get_block_hash(u64::from(publication_height))?;
    let block = rt.rpc.get_block(&publication_hash)?;
    let index = block
        .txdata
        .iter()
        .position(|tx| tx.compute_txid() == txid)
        .context("activation transaction index")?;
    // Cross the actual Core 30 default-regtest 144-block boundary, not older
    // library parameters. Activation confirmations may mature after expiry.
    rt.mine(150)
        .context("mine through Core 30 regtest boundary")?;
    let headers: Vec<_> = (height + 1..=rt.height()?)
        .map(|h| -> Result<_> {
            let hash = rt
                .rpc
                .get_block_hash(u64::from(h))
                .with_context(|| format!("read chain height {h}"))?;
            Ok(serialize(&rt.rpc.get_block_header(&hash)?)
                .try_into()
                .expect("80-byte header"))
        })
        .collect::<Result<_>>()?;
    let chain = Chain::verify(checkpoint.clone(), &checkpoint.commitment(), headers, 200)?;
    let raw = serialize(&block);
    let inclusion = Inclusion::capture(&raw, index, 4_000_000)?;
    let included = inclusion.verify(&chain, publication_height, 100_000)?;
    ensure!(
        included.transaction().txid() == txid.to_byte_array(),
        "native txid matches Core"
    );
    ensure!(
        included.input_witness(0).is_err(),
        "txid-only proof has no witness authority"
    );
    let witness = WitnessInclusion::capture(&raw, index, 4_000_000)?;
    let included = witness.verify(&chain, publication_height, 100_000)?;
    ensure!(
        included.input_witness(0)? == block.txdata[index].input[0].witness.to_vec(),
        "BIP141 authenticates Core's actual witness"
    );
    let activation = Activation {
        reference: ActivationRef {
            height: publication_height,
            block_hash: publication_hash.to_byte_array(),
            txid: txid.to_byte_array(),
            output: activation_output,
        },
        inclusion,
    };
    let terms = TimedTerms::new(
        ChainProfile::Core30DefaultRegtest,
        checkpoint.commitment(),
        height + 2,
        3,
        sha256::Hash::hash(&secret).to_byte_array(),
    )?;
    ensure!(
        activation.verify(&chain, &terms, 100_000)? == secret,
        "native positive activation"
    );
    ensure!(
        chain.tip_height() > terms.expiry(),
        "maturity after expiry remains valid for early inclusion"
    );
    ensure!(
        publication_height + terms.depth() - 1 > terms.expiry(),
        "required confirmations mature after expiry"
    );
    Ok(())
}
