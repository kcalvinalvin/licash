//! Licash on Bitcoin, outside the audited protocol library: silent payment
//! addresses (BIP352) that wallets receive coins at, and the deposits that
//! turn coins into a hub's notes.

pub mod deposit;
pub mod silent;

use bitcoin::Network;
use licash::spend::ChainProfile;

/// The network whose addresses the chain of a checkpoint `profile` uses.
pub fn network(profile: ChainProfile) -> Network {
    match profile {
        ChainProfile::Core30Mainnet => Network::Bitcoin,
        ChainProfile::Core30DefaultRegtest => Network::Regtest,
        ChainProfile::Core30Signet | ChainProfile::Mutinynet => Network::Signet,
    }
}
