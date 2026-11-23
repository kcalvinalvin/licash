import Chain
import Foundation
import LicashCore

/// The Go light client as the wallet reads Bitcoin through it.
final class GoChain: ChainSource, @unchecked Sendable {
    // Go's light client may be called from any thread.
    private let light: MobileChain

    init(light: MobileChain) {
        self.light = light
    }

    func height() throws -> UInt32 {
        try call {
            var height: Int32 = 0
            try light.height(&height)
            return UInt32(bitPattern: height)
        }
    }

    func hash(height: UInt32) throws -> Data { try call { try light.hash(Int32(bitPattern: height)) } }

    func header(height: UInt32) throws -> Data { try call { try light.header(Int32(bitPattern: height)) } }

    func time(height: UInt32) throws -> Int64 {
        try call {
            var time: Int64 = 0
            try light.time(Int32(bitPattern: height), ret0_: &time)
            return time
        }
    }

    func filter(hash: Data) throws -> Data { try call { try light.filter(hash) } }

    func block(hash: Data) throws -> Data { try call { try light.block(hash) } }

    func broadcast(tx: Data) throws { try call { try light.broadcast(tx) } }

    func unconfirmed(script: Data) throws -> Data {
        try call {
            do {
                return try light.unconfirmed(script)
            } catch let e as Universeerror {
                throw e
            } catch {
                // Nothing relayed: gomobile hands the empty result over as
                // nil, which Swift throws as an error that is not Go's.
                return Data()
            }
        }
    }

    private func call<T>(f: () throws -> T) throws -> T {
        do {
            return try f()
        } catch {
            throw ChainError.Failed(
                reason: (error as NSError).userInfo[NSLocalizedDescriptionKey] as? String ?? "the light client failed"
            )
        }
    }
}

/// No light client running: whatever needs Bitcoin fails, plainly.
enum NoChain {
    static let instance: ChainSource = Failing()

    private final class Failing: ChainSource {
        private func none() throws -> Never { throw ChainError.Failed(reason: "Bitcoin is not reached yet") }

        func height() throws -> UInt32 { try none() }

        func hash(height: UInt32) throws -> Data { try none() }

        func header(height: UInt32) throws -> Data { try none() }

        func time(height: UInt32) throws -> Int64 { try none() }

        func filter(hash: Data) throws -> Data { try none() }

        func block(hash: Data) throws -> Data { try none() }

        func broadcast(tx: Data) throws { try none() }

        func unconfirmed(script: Data) throws -> Data { try none() }
    }
}
