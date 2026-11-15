//! Format 1 frozen: an opening's states at Funded, the holder's and the
//! hub's, as the first release storing them did, from a mock opening on
//! regtest (`apps/hub/tests/contracts.rs`, the wallet of seed `[7; 32]`).
//! Each loads, stores itself again byte for byte, and is of the contract
//! both signed. A change that breaks this strands contracts in the field.

use k256::elliptic_curve::PrimeField;
use k256::{FieldBytes, Scalar};
use licash_contract::opening::{Context, Holder, Hub, HubPhase, Phase, Trust};

const HOLDER: &[u8] = include_bytes!("golden/holder-funded.bin");
const HUB: &[u8] = include_bytes!("golden/hub-funded.bin");

/// The holder's contract secrets, of her wallet's seed.
const SEED: &str = "2c45ef335c38f0e7417c97395c863c450c35fa34a629267e999994292aaa9ee7";
const SPEND: &str = "548fedbbc7e1fb67570cac75bf01a723c31987d60bfcd44eafd8c1da39b7c54f";

/// The contract's final digest, D, and F's id, as stored.
const D: &str = "23d8eae190059b5980f12f7a57e0729c0a0396f60b918fd556e04b852be688e5";
const F: &str = "d3bb61f8268d212378c96ae6a216197fb99539c986a1fe372e121bad96ff85c6";

fn bytes<const N: usize>(text: &str) -> [u8; N] {
    assert_eq!(text.len(), 2 * N, "{text}");
    let mut out = [0; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Any tip: a load reads none.
fn ctx() -> Context {
    Context {
        tip: 0,
        trust: Trust::mock(),
    }
}

#[test]
fn a_holder_funded_in_format_1_loads_as_stored() {
    let spend = Scalar::from_repr(FieldBytes::from(bytes::<32>(SPEND)));
    let spend = Option::<Scalar>::from(spend).expect("a scalar");
    let holder = Holder::load(HOLDER, bytes(SEED), spend, &ctx()).unwrap();
    assert_eq!(holder.phase(), Phase::Funded);
    assert!(!holder.abandonable());
    assert_eq!(holder.to_bytes(), HOLDER, "stored otherwise");
    let txid = holder.funding().unwrap().tx.compute_txid();
    assert_eq!(txid.to_string(), F);
    let opening = holder.opening().unwrap();
    assert_eq!(hex(opening.d.as_bytes()), D);
}

#[test]
fn a_hub_funded_in_format_1_loads_as_stored() {
    let hub = Hub::load(HUB, &ctx()).unwrap();
    assert_eq!(hub.phase(), Some(HubPhase::Funded));
    assert!(!hub.abandonable());
    assert_eq!(hub.to_bytes().unwrap(), HUB, "stored otherwise");
    let txid = hub.funding().unwrap().tx.compute_txid();
    assert_eq!(txid.to_string(), F);
    let opening = hub.opening().unwrap();
    assert_eq!(hex(opening.d.as_bytes()), D);
}
