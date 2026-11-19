//! licash's recovery contract on Lichen's BABE graph (`lichen_babe::graph`)
//! behind Licash's front, where the hub proves: the holder requests what she claims and publishes
//! her claim, the hub responds with what she is owed and proves it, and
//! she challenges a false proof with the instances she garbled at opening.
//!
//! `graph` builds the graph from Base and both parties' keys (`keys`);
//! `opening` opens it between a holder and a hub over `wire`'s messages;
//! `dispute` is each side's moves on the chain, `exit` what its statement
//! (`licash_statement`) reads of the exit's blocks. The regtest suite
//! (`regtest/`) plays it on Bitcoin Core.

pub mod dispute;
pub mod exit;
pub mod graph;
pub mod keys;
pub mod opening;
pub mod reserve;
pub mod wire;

/// The most instances a holder garbles for an opening: what the hub may
/// ask of her, as each costs her a garbling (and the kept ones an upload).
pub const MAX_INSTANCES: u16 = 128;
