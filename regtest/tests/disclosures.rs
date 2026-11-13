use anyhow::Result;
use licash::disclosure::{Disclosures, CHUNK_BYTES};
use licash_regtest::native_fixture::fixture;
use lngap_btc::regtest::Regtest;

#[test]
fn disclosure_signatures_preserve_key_order_and_chunk_boundaries() -> Result<()> {
    let rt = Regtest::start_standard()?;
    let f = fixture(&rt, "disclosure-signatures")?;
    let values = Disclosures {
        values: std::array::from_fn(|i| [i as u8 + 1; 32]),
    };
    let signed = f.c.sign_disclosures(&values)?;
    let keys = f.c.p.hub_keys().disclosure;
    assert_eq!(signed.disclosures, values);
    assert!(signed.verify(&keys));

    let bytes = values.to_bytes();
    let mut offset = 0;
    for ((signature, key), size) in signed
        .signatures
        .iter()
        .zip(&f.c.p.disclosure)
        .zip(CHUNK_BYTES)
    {
        let end = offset + size;
        assert_eq!(
            *signature,
            licash_regtest::sig(&key.sign(&bytes[offset..end])?)
        );
        offset = end;
    }
    assert_eq!(offset, bytes.len());

    let mut reordered = signed.clone();
    reordered.signatures.swap(0, 1);
    assert!(!reordered.verify(&keys));

    let mut changed = signed;
    changed.disclosures.values[31][31] ^= 1;
    assert!(!changed.verify(&keys));
    Ok(())
}
