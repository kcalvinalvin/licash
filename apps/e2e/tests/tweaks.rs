//! Hubs index the silent payment tweaks of the blocks Bitcoin Core makes,
//! on the stack of `apps/e2e/run.sh`, which runs these.

use anyhow::Context;
use bitcoin::{consensus::deserialize, Script, ScriptBuf, Transaction};
use licash_api::BlockTweaks;
use licash_e2e::{until, Eco};
use licash_onchain::silent;
use serde_json::json;

#[tokio::test]
#[ignore = "needs the stack of apps/e2e/run.sh"]
async fn hubs_serve_the_tweak_of_a_payment_to_a_taproot_output() -> anyhow::Result<()> {
    let eco = Eco::up().await?;
    let address = eco.miner_address("bech32m").await?;
    let txid = eco.send_to(&address, 100_000).await?;
    let height = eco.mine(1).await?;
    let hash = eco.core("getblockhash", json!([height])).await?;
    let hash = hash.as_str().context("a block hash")?.to_owned();
    // The tweak, from what Bitcoin Core tells of the transaction.
    let verbose = eco
        .core("getrawtransaction", json!([txid, 2, hash]))
        .await?;
    let raw = hex::decode(verbose["hex"].as_str().context("a raw transaction")?)?;
    let tx: Transaction = deserialize(&raw)?;
    let prevouts = verbose["vin"]
        .as_array()
        .context("inputs")?
        .iter()
        .map(|vin| {
            let hex = vin["prevout"]["scriptPubKey"]["hex"].as_str();
            Ok(ScriptBuf::from_hex(hex.context("a spent script")?)?)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let scripts: Vec<&Script> = prevouts.iter().map(ScriptBuf::as_script).collect();
    let tweak = silent::tweak(&tx, &scripts).context("a payment that may be silent")?;

    for n in [1, 2] {
        let hub = eco.hub(n);
        let first = eco.info(hub).await?.tweaks.context("tweaks served")?;
        assert!(first < height, "hub{n} indexes from {first}");
        let url = format!("{hub}{}?from={height}&count=1", licash_api::TWEAKS);
        let blocks: Vec<BlockTweaks> = until(&format!("hub{n} at {height}"), 60, || async {
            let blocks: Vec<BlockTweaks> = reqwest::get(&url).await?.json().await?;
            Ok((!blocks.is_empty()).then_some(blocks))
        })
        .await?;
        assert_eq!(blocks[0].hash, hash);
        let served: Vec<_> = blocks[0]
            .tweaks
            .iter()
            .map(|t| bitcoin::secp256k1::PublicKey::from_slice(&t.0))
            .collect::<Result<_, _>>()?;
        assert!(served.contains(&tweak), "hub{n}: {served:?} lacks {tweak}");
        // Not a range beneath the first block indexed, if any.
        if let Some(below) = first.checked_sub(1) {
            let below = format!("{hub}{}?from={below}&count=1", licash_api::TWEAKS);
            assert_eq!(reqwest::get(&below).await?.status(), 400);
        }
    }
    Ok(())
}
