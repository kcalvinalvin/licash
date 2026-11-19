//! Licash on Bitcoin, outside the audited protocol library: silent payment
//! addresses (BIP352) that wallets receive coins at, and the hash-locked
//! coins of deposits, which turn coins into a hub's notes, and of payouts,
//! which a swap provider pays a wallet's notes out with.

pub mod deadline;
pub mod deposit;
pub mod key_agg;
pub mod silent;
pub mod silent_claim;

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
