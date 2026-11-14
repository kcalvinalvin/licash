//! licash's recovery contract on Lichen's BABE transaction graph: `graph`
//! builds and presigns the graph, `opening` opens it between a holder and
//! a hub, each with its own `keys`, over `wire`'s messages. The regtest
//! suite (`regtest/`) plays it on Bitcoin Core.

pub mod graph;
pub mod keys;
pub mod opening;
pub mod wire;

/// Instances the hub builds of each assertion, and how many the holder
/// keeps.
pub const INSTANCES: usize = 4;
pub const KEPT: usize = 2;
