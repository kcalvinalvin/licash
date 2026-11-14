//! Payment codes: one text, or QR code, a payer scans to pay a wallet in
//! notes or on Bitcoin, whichever it can.
//!
//! A code is a BIP321 URI,
//! `bitcoin:<address>?sp=<silent payment address>&licash=<code>`, which any
//! wallet can pay on Bitcoin: at the silent payment address if it pays
//! them, else at the address, the wallet's fallback address. Its
//! `licash` parameter is bech32m (BIP350), at most 1023 characters, under
//! `licash` on mainnet and `tlicash` on the other networks, of version 0:
//! the network's byte, the 99 bytes of the wallet's [`Address`], then
//! records of a type byte, a length byte and the value, in non-decreasing
//! order of type:
//!
//! - 1, once at most: the x-only Nostr key of the wallet's mailbox, where
//!   payers send it payments (NIP-17), on the relays its hubs name.
//! - 2, any number up to [`MAX_HUBS`]: a hub the wallet is paid at
//!   ([`HubId`]), in its order of preference.
//!
//! Records of other types are skipped; a code of a later version is
//! ignored, leaving its payer the rest of the URI. A hub named in a code is
//! no reason to trust it: a payer pays at a hub it trusts already.

use std::{fmt, str::FromStr};

use bitcoin::{
    address::NetworkUnchecked,
    bech32::{
        primitives::{
            decode::CheckedHrpstring,
            iter::{ByteIterExt, Fe32IterExt},
        },
        Bech32m, Fe32, Hrp,
    },
    secp256k1::{Secp256k1, SecretKey},
    Amount, Denomination, NetworkKind,
};
use licash::{address::Address, crypto::tagged_hash, spend::ChainProfile};
use licash_onchain::silent::SpAddress;

use crate::{Error, Transport, Wallet};

/// The most hubs a code names that a payer reads.
pub const MAX_HUBS: usize = 8;

const MAILBOX: u8 = 1;
const HUB: u8 = 2;

/// A hub, as payment codes name it: the first 16 bytes of the tagged hash
/// of the address in its info. Its URLs may change, and differ between
/// wallets (one over Tor, one not); its address does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HubId(pub [u8; 16]);

impl HubId {
    /// The hub whose info names `address`.
    pub fn of(address: &Address) -> Self {
        let hash = tagged_hash("licash/code/hub", &[&address.to_bytes()]);
        let mut id = [0; 16];
        id.copy_from_slice(&hash[..16]);
        Self(id)
    }
}

/// What a payment code asks to be paid with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentCode {
    /// An address on Bitcoin, as in any BIP21 URI.
    pub address: Option<bitcoin::Address<NetworkUnchecked>>,
    /// In satoshis.
    pub amount: Option<u64>,
    /// A silent payment address (BIP352).
    pub silent: Option<SpAddress>,
    pub licash: Option<LicashCode>,
}

/// The Licash part of a payment code: the wallet to pay in notes, where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LicashCode {
    pub network: ChainProfile,
    pub address: Address,
    /// The x-only Nostr key payments are sent to.
    pub mailbox: Option<[u8; 32]>,
    /// The hubs it is paid at, preferred first; at most [`MAX_HUBS`] are
    /// written.
    pub hubs: Vec<HubId>,
}

/// A network's byte in codes, as in the protocol's encodings.
fn network_byte(network: ChainProfile) -> u8 {
    match network {
        ChainProfile::Core30Mainnet => 0,
        ChainProfile::Core30DefaultRegtest => 1,
        ChainProfile::Core30Signet => 3,
        ChainProfile::Mutinynet => 4,
    }
}

fn network_kind(network: ChainProfile) -> NetworkKind {
    match network {
        ChainProfile::Core30Mainnet => NetworkKind::Main,
        _ => NetworkKind::Test,
    }
}

fn invalid(why: impl fmt::Display) -> Error {
    Error::Invalid(format!("not a payment code: {why}"))
}

impl LicashCode {
    fn hrp(network: ChainProfile) -> Hrp {
        let hrp = match network_kind(network) {
            NetworkKind::Main => "licash",
            NetworkKind::Test => "tlicash",
        };
        Hrp::parse(hrp).expect("a valid hrp")
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![network_byte(self.network)];
        bytes.extend(self.address.to_bytes());
        if let Some(mailbox) = &self.mailbox {
            bytes.extend([MAILBOX, 32]);
            bytes.extend(mailbox);
        }
        for hub in self.hubs.iter().take(MAX_HUBS) {
            bytes.extend([HUB, 16]);
            bytes.extend(hub.0);
        }
        bytes
    }

    /// The code `s` writes, or none if of a later version.
    fn decode(s: &str) -> Result<Option<Self>, Error> {
        let checked = CheckedHrpstring::new::<Bech32m>(s).map_err(invalid)?;
        let test = match checked.hrp().to_lowercase().as_str() {
            "licash" => false,
            "tlicash" => true,
            hrp => return Err(invalid(format!("prefix {hrp}"))),
        };
        let data = checked.data_part_ascii_no_checksum();
        let (version, data) = data.split_first().ok_or_else(|| invalid("empty"))?;
        let version = Fe32::from_char((*version).into()).map_err(invalid)?;
        if version != Fe32::Q {
            return Ok(None);
        }
        let bytes = fes_to_bytes(data)?;
        let (&network, rest) = bytes.split_first().ok_or_else(|| invalid("empty"))?;
        let network = ChainProfile::ALL
            .into_iter()
            .find(|n| network_byte(*n) == network)
            .ok_or_else(|| invalid(format!("network {network}")))?;
        if test != (network_kind(network) == NetworkKind::Test) {
            return Err(invalid(format!(
                "a code of {network} under {}",
                checked.hrp()
            )));
        }
        if rest.len() < 99 {
            return Err(invalid("too short"));
        }
        let (address, mut records) = rest.split_at(99);
        let mut code = Self {
            network,
            address: Address::from_bytes(address).map_err(invalid)?,
            mailbox: None,
            hubs: Vec::new(),
        };
        let mut last = 0;
        while let [kind, len, rest @ ..] = records {
            let (kind, len) = (*kind, usize::from(*len));
            if kind < last || rest.len() < len {
                return Err(invalid("records out of order or cut short"));
            }
            let (value, rest) = rest.split_at(len);
            match kind {
                MAILBOX if last == MAILBOX || len != 32 => return Err(invalid("mailbox")),
                MAILBOX => code.mailbox = Some(value.try_into().expect("32 bytes")),
                HUB if len != 16 || code.hubs.len() == MAX_HUBS => return Err(invalid("hubs")),
                HUB => code.hubs.push(HubId(value.try_into().expect("16 bytes"))),
                _ => {}
            }
            (last, records) = (kind, rest);
        }
        if !records.is_empty() {
            return Err(invalid("a record cut short"));
        }
        Ok(Some(code))
    }
}

/// Bytes of bech32 characters, their padding zero.
fn fes_to_bytes(chars: &[u8]) -> Result<Vec<u8>, Error> {
    let (mut acc, mut bits, mut bytes) = (0u32, 0u32, Vec::with_capacity(chars.len() * 5 / 8));
    for c in chars {
        let fe = Fe32::from_char((*c).into()).map_err(invalid)?;
        acc = (acc << 5) | u32::from(fe.to_u8());
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
        acc &= (1 << bits) - 1;
    }
    if bits >= 5 || acc != 0 {
        return Err(invalid("padding"));
    }
    Ok(bytes)
}

impl fmt::Display for LicashCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hrp = Self::hrp(self.network);
        let chars = self
            .to_bytes()
            .into_iter()
            .bytes_to_fes()
            .with_checksum::<Bech32m>(&hrp)
            .with_witness_version(Fe32::Q)
            .chars();
        chars
            .into_iter()
            .try_for_each(|c| fmt::Write::write_char(f, c))
    }
}

impl FromStr for LicashCode {
    type Err = Error;

    /// A code of version 0: one of a later version is refused.
    fn from_str(s: &str) -> Result<Self, Error> {
        Self::decode(s)?.ok_or_else(|| invalid("a later version"))
    }
}

impl PaymentCode {
    /// The code to show as a QR code: in upper case, which QR codes hold
    /// in fewer modules, unless its address is not bech32.
    pub fn qr(&self) -> String {
        let text = self.to_string();
        let bech32 = self.address.as_ref().is_none_or(|a| {
            let a = a.assume_checked_ref().to_string();
            a.starts_with("bc1") || a.starts_with("tb1") || a.starts_with("bcrt1")
        });
        match bech32 {
            true => text.to_uppercase(),
            false => text,
        }
    }

    /// Refused unless all of it is of `network`: a payment on another
    /// chain is lost to the payee, so not even the rest is paid.
    pub fn check(&self, network: ChainProfile) -> Result<(), Error> {
        let wrong = |what: &str| Err(Error::Invalid(format!("{what} not on {network}")));
        if self.licash.as_ref().is_some_and(|c| c.network != network) {
            return wrong("a payment code");
        }
        if self
            .silent
            .is_some_and(|s| s.network != network_kind(network))
        {
            return wrong("a silent payment address");
        }
        let bitcoin = licash_onchain::network(network);
        if self
            .address
            .as_ref()
            .is_some_and(|a| !a.is_valid_for_network(bitcoin))
        {
            return wrong("an address");
        }
        Ok(())
    }
}

impl fmt::Display for PaymentCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bitcoin:")?;
        if let Some(address) = &self.address {
            write!(f, "{}", address.assume_checked_ref())?;
        }
        let mut separator = '?';
        let mut param = |f: &mut fmt::Formatter<'_>, key: &str, value: &dyn fmt::Display| {
            let written = write!(f, "{separator}{key}={value}");
            separator = '&';
            written
        };
        if let Some(amount) = self.amount {
            let btc = Amount::from_sat(amount).to_string_in(Denomination::Bitcoin);
            param(f, "amount", &btc)?;
        }
        if let Some(silent) = &self.silent {
            param(f, "sp", silent)?;
        }
        if let Some(licash) = &self.licash {
            param(f, "licash", licash)?;
        }
        Ok(())
    }
}

impl FromStr for PaymentCode {
    type Err = Error;

    /// A BIP321 URI, or a bare address: a silent payment address, a Licash
    /// code or an address on Bitcoin. Refused if it asks for something
    /// this does not know (a `req-` parameter) or for nothing it can pay.
    fn from_str(s: &str) -> Result<Self, Error> {
        let s = s.trim();
        let mut code = Self {
            address: None,
            amount: None,
            silent: None,
            licash: None,
        };
        let uri = s
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("bitcoin:"));
        if !uri {
            let lower = s.to_lowercase();
            if lower.starts_with("sp1") || lower.starts_with("tsp1") {
                code.silent = Some(s.parse().map_err(invalid)?);
            } else if lower.starts_with("licash1") || lower.starts_with("tlicash1") {
                code.licash = Some(s.parse()?);
            } else {
                code.address = Some(s.parse().map_err(invalid)?);
            }
            return Ok(code);
        }
        let (path, query) = s[8..].split_once('?').unwrap_or((&s[8..], ""));
        if !path.is_empty() {
            code.address = Some(path.parse().map_err(invalid)?);
        }
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = key.to_lowercase();
            let twice = || invalid(format!("{key} twice"));
            match key.as_str() {
                "amount" if code.amount.is_some() => return Err(twice()),
                "amount" => {
                    let amount = Amount::from_str_in(value, Denomination::Bitcoin);
                    code.amount = Some(amount.map_err(invalid)?.to_sat());
                }
                "sp" if code.silent.is_some() => return Err(twice()),
                "sp" => code.silent = Some(value.parse().map_err(invalid)?),
                "licash" if code.licash.is_some() => return Err(twice()),
                "licash" => code.licash = LicashCode::decode(value)?,
                _ if key.starts_with("req-") => {
                    return Err(invalid(format!("it requires {key}")));
                }
                _ => {}
            }
        }
        if code.address.is_none() && code.silent.is_none() && code.licash.is_none() {
            return Err(invalid("nothing to pay"));
        }
        Ok(code)
    }
}

/// The keys a wallet's seed derives besides its owner's: its silent
/// payment keys and its mailbox's.
pub(crate) struct CodeKeys {
    pub(crate) silent: licash_onchain::silent::Keys,
    pub(crate) mailbox: SecretKey,
}

impl CodeKeys {
    pub(crate) fn from_seed(seed: &[u8; 32], network: ChainProfile) -> Result<Self, Error> {
        let silent = licash_onchain::silent::Keys::from_seed(seed, network_kind(network))
            .map_err(|e| Error::Setup(format!("silent payment keys: {e}")))?;
        let mailbox = tagged_hash("licash/mailbox", &[seed]);
        let mailbox = SecretKey::from_slice(&mailbox)
            .map_err(|_| Error::Setup("no mailbox key from this seed".into()))?;
        Ok(Self { silent, mailbox })
    }
}

impl<T: Transport> Wallet<T> {
    /// The code payers pay this wallet with: at the hubs of `hubs`, which
    /// it trusts, preferred first, each once, and its
    /// mailbox if one of them names relays; on Bitcoin if `silent`, for
    /// wallets that scan for it: at its silent payment address, and at a
    /// fallback address for payers of no silent payments, handed out and
    /// watched from then on, the next once it is paid.
    pub fn payment_code(&self, hubs: &[String], silent: bool) -> Result<PaymentCode, Error> {
        let trusted = self.hubs()?;
        let mut ids = Vec::new();
        let mut relays = false;
        for url in hubs {
            let url = crate::hub_url(url)?;
            let (_, info) = trusted
                .iter()
                .find(|(u, _)| *u == url)
                .ok_or_else(|| Error::UnknownHub(url.clone()))?;
            let id = HubId::of(&info.address);
            if !ids.contains(&id) {
                ids.push(id);
            }
            relays |= !info.relays.is_empty();
        }
        let mailbox = relays.then(|| self.keys.mailbox.x_only_public_key(&Secp256k1::new()).0);
        let address = match silent {
            true => Some(self.fallback_address()?.as_unchecked().clone()),
            false => None,
        };
        Ok(PaymentCode {
            address,
            amount: None,
            silent: silent.then(|| self.keys.silent.address()),
            licash: Some(LicashCode {
                network: self.network,
                address: self.address(),
                mailbox: mailbox.map(|key| key.serialize()),
                hubs: ids,
            }),
        })
    }

    /// The secret key of the wallet's mailbox, which opens what payers
    /// send it (NIP-17).
    pub fn mailbox_secret(&self) -> [u8; 32] {
        self.keys.mailbox.secret_bytes()
    }

    /// The wallet's silent payment keys, to scan for its outputs and spend
    /// them.
    pub fn silent_keys(&self) -> &licash_onchain::silent::Keys {
        &self.keys.silent
    }

    /// The hub to pay `amount` at to the payee of `code`: the first it
    /// names that this wallet trusts and holds that much at, the hub's fee
    /// on top, if any.
    pub fn hub_to_pay(&self, code: &LicashCode, amount: u64) -> Result<Option<String>, Error> {
        if code.network != self.network {
            return Err(Error::Invalid(format!(
                "a payment code of {}, not {}",
                code.network, self.network
            )));
        }
        let trusted = self.hubs()?;
        for id in &code.hubs {
            let Some((url, _)) = trusted.iter().find(|(_, i)| HubId::of(&i.address) == *id) else {
                continue;
            };
            let (hub, info) = self.hub(url)?;
            match self.select(hub, amount, &info.fees.spend) {
                Ok(_) => return Ok(Some(url.clone())),
                Err(Error::Insufficient { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }
}
