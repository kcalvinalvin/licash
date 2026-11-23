import Foundation
import LicashCore

/// A margin in days, as blocks: Bitcoin's 144 a day on every network, its
/// safety counted in blocks like the contracts' own. Not a wall-clock
/// promise: on Mutinynet, whose blocks come 20 times as quick, it is the
/// same blocks and so a twentieth of the time.
func exitMarginBlocks(days: Int) -> UInt32 {
    precondition((1...30).contains(days))
    return UInt32(days) * BLOCKS_PER_DAY
}

private let BLOCKS_PER_DAY: UInt32 = 144

/// How often a forced close needs the wallet online: its signed challenge
/// window of `blocks`, of `blockSecs` each, in estimated hours.
func checkInHours(blocks: UInt32, blockSecs: UInt32) -> UInt32 {
    max(1, UInt32(UInt64(blocks) * UInt64(blockSecs) / 3600))
}

/// The displayed deadline is for R to confirm, not for its broadcast to start.
func safetyStatus(safety: ContractSafety, height: Int) -> String {
    if height <= 0 { return "Chain height unknown" }
    guard let requestBy = safety.requestBy else { return "Margin does not fit this contract" }
    if height > Int(requestBy) { return "Inside your recovery margin" }
    return "Request should confirm by block \(requestBy)"
}
