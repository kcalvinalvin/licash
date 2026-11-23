import LicashCore
import XCTest
@testable import Licash

class SyncTest: XCTestCase {
    private func hub() -> Hub {
        Hub(
            url: "https://hub.example", feeBase: 0, feePpm: 0, feeOutputMsat: 0, deposits: nil, silent: false,
            mailbox: false,
            lightningPay: nil, lockFee: Rate(base: 0, ppm: 0), refundFee: Rate(base: 0, ppm: 0), depositFee: nil,
            lightning: nil, contracts: nil
        )
    }

    private func scan(from: UInt32, next: UInt32, height: UInt32, tweaks: Bool = true) -> Scan {
        Scan(from: from, next: next, height: height, tweaks: tweaks, found: 0, foundValue: 0)
    }

    private let caughtUp = ChainStatus(height: 900, peers: 2, synced: true, headers: 900, peerHeight: 900)

    func test_a_fraction_only_on_a_known_way() {
        XCTAssertEqual(fraction(from: 0, done: 50, to: 100)!, 0.5, accuracy: 0.0001)
        XCTAssertEqual(fraction(from: 100, done: 125, to: 200)!, 0.25, accuracy: 0.0001)
        XCTAssertNil(fraction(from: 0, done: 10, to: 0))
        XCTAssertNil(fraction(from: 0, done: 150, to: 100))
        XCTAssertNil(fraction(from: 100, done: 50, to: 200))
    }

    func test_a_new_wallet_waits_for_each_step_before_it() {
        let ui = Ui(hubs: [hub()], chain: ChainStatus(peers: 1, headers: 400, peerHeight: 800))
        let stages = stages(ui: ui)
        XCTAssertEqual(stages.map { $0.name }, ["Bitcoin nodes", "Block headers", "Block filters", "Your payments"])
        XCTAssertEqual(stages.map { $0.state }, [.done, .doing, .waiting, .waiting])
        XCTAssertEqual(stages[1].detail, "Block 400 of 800 (50%)")
        XCTAssertEqual(stages[1].fraction!, 0.5, accuracy: 0.0001)
        // Waiting: no fraction shown, nor where it would be.
        XCTAssertNil(stages[2].fraction)
        XCTAssertEqual(stages.dropFirst(2).map { $0.detail }, ["Waiting", "Waiting"])
        XCTAssertTrue(catchingUp(ui: ui))
    }

    func test_settings_says_the_pinned_headers_download_and_set_up_not_connecting() {
        XCTAssertEqual(status(ui: Ui(chain: ChainStatus(importing: 42)), developer: false).0, "Downloading")
        XCTAssertEqual(status(ui: Ui(chain: ChainStatus(importing: 100)), developer: false).0, "Setting up")
        XCTAssertEqual(status(ui: Ui(chain: ChainStatus()), developer: false).0, "Connecting")
    }

    func test_the_pinned_headers_download_before_any_node_is_looked_for() {
        let ui = Ui(hubs: [hub()], chain: ChainStatus(importing: 42))
        let stages = stages(ui: ui)
        XCTAssertEqual(
            stages.map { $0.name },
            ["Block headers download", "Bitcoin nodes", "Block headers", "Block filters", "Your payments"]
        )
        XCTAssertEqual(stages[0].state, .doing)
        XCTAssertEqual(stages[0].detail, "Downloading (42%)")
        XCTAssertEqual(stages[0].fraction!, 0.42, accuracy: 0.0001)
        XCTAssertTrue(stages.dropFirst(1).allSatisfy { $0.state == .waiting })
        XCTAssertTrue(catchingUp(ui: ui))
    }

    func test_downloaded_the_pinned_headers_are_prepared_for_a_time_untold() {
        let stage = stages(ui: Ui(hubs: [hub()], chain: ChainStatus(importing: 100))).first!
        XCTAssertEqual(stage.detail, PREPARING)
        // Downloaded: no longer named a download.
        XCTAssertEqual(stage.name, HEADERS_SETUP)
        XCTAssertEqual(said(stage: stage, developer: false), PREPARING)
        XCTAssertNil(stage.fraction)
        XCTAssertTrue(stage.timed)
        XCTAssertFalse(stages(ui: Ui(chain: ChainStatus(importing: 99))).first!.timed)
    }

    func test_checked_the_pinned_headers_are_stored_as_far_as_told() {
        let stage = stages(ui: Ui(hubs: [hub()], chain: ChainStatus(importing: 100, storing: 35))).first!
        XCTAssertEqual(stage.detail, "Storing (35%)")
        XCTAssertEqual(stage.fraction!, 0.35, accuracy: 0.0001)
        XCTAssertTrue(stage.timed)
        XCTAssertEqual(said(stage: stage, developer: false), "Storing block headers (35%)")
        // Not told yet: still checked, for a time untold.
        XCTAssertEqual(stages(ui: Ui(chain: ChainStatus(importing: 100, storing: -1))).first!.detail, PREPARING)
    }

    func test_only_a_new_wallets_first_catching_up_is_shown_on_home() {
        let behind = Ui(hubs: [hub()], chain: ChainStatus(peers: 1, headers: 400, peerHeight: 800))
        XCTAssertTrue(firstCatchingUp(ui: behind))
        // Caught up once: the connections dot tells of it from then on.
        XCTAssertFalse(firstCatchingUp(ui: { var c = behind; c.caughtUp = true; return c }()))
        XCTAssertFalse(firstCatchingUp(ui: Ui(hubs: [hub()], chain: caughtUp, scan: scan(from: 800, next: 901, height: 900))))
    }

    func test_the_connections_say_how_long_they_took_to_answer() {
        XCTAssertEqual(latency(ms: 85), "85 ms")
        XCTAssertEqual(latency(ms: 1_240), "1,240 ms")
        let ui = Ui(
            hubs: [{ var c = hub(); c.mailbox = true; return c }()],
            chain: { var c = caughtUp; c.pingMs = 120; return c }(),
            scan: scan(from: 800, next: 901, height: 900),
            hubsReached: ["https://hub.example": true],
            hubsMs: ["https://hub.example": 640],
            relaysReached: true,
            relaysMs: 900
        )
        XCTAssertEqual(links(ui: ui).map { $0.ms }, [120, 640, 900])
        // None for a hub not answering, nor for Bitcoin with no node.
        var down = ui
        down.hubsReached = ["https://hub.example": false]
        down.chain = { var c = caughtUp; c.pingMs = 120; c.peers = 0; return c }()
        XCTAssertEqual(links(ui: down).prefix(2).map { $0.ms }, [nil, nil])
    }

    func test_the_step_under_way_is_said_plainly_and_in_full_in_advanced_mode() {
        let ui = Ui(hubs: [hub()], chain: ChainStatus(peers: 1, headers: 400, peerHeight: 800))
        let step = stages(ui: ui)[1]
        XCTAssertEqual(said(stage: step, developer: false), "Catching up with Bitcoin (50%)")
        XCTAssertEqual(said(stage: step, developer: true), "Block headers: Block 400 of 800 (50%)")
        let importing = stages(ui: Ui(chain: ChainStatus(importing: 42))).first!
        XCTAssertEqual(said(stage: importing, developer: false), "Downloading block headers (42%)")
        let nodes = stages(ui: Ui(chain: ChainStatus())).first!
        XCTAssertEqual(said(stage: nodes, developer: false), "Looking for Bitcoin nodes")
        let hub = stages(ui: Ui(hubs: [hub()], chain: caughtUp, scan: scan(from: 800, next: 896, height: 895))).last!
        XCTAssertEqual(said(stage: hub, developer: false), "Checking the newest block")
    }

    func test_the_connections_are_tor_if_on_then_bitcoin() {
        // Tor and Bitcoin alone: the hubs and relays are said after them.
        func chain(ui: Ui) -> [Link] { links(ui: ui).filter { $0.name == TOR || $0.name == BITCOIN } }
        let hubs = [hub()]
        let done = scan(from: 800, next: 901, height: 900)
        XCTAssertEqual(chain(ui: Ui(hubs: hubs, chain: caughtUp, scan: done)), [Link(name: "Bitcoin", reach: .up, detail: "Up to date")])
        XCTAssertEqual(
            chain(ui: Ui(developer: true, hubs: hubs, chain: caughtUp, scan: done)),
            [Link(name: "Bitcoin", reach: .up, detail: "Up to date, block 900, 2 nodes")]
        )
        // No hub yet: payments are not looked for, and Bitcoin is not behind.
        XCTAssertEqual(chain(ui: Ui(chain: caughtUp)).map { $0.reach }, [.up])
        XCTAssertEqual(chain(ui: Ui(hubs: hubs)), [Link(name: "Bitcoin", reach: .going, detail: "Looking for Bitcoin nodes")])

        let ready = TorStatus(percent: 100, ready: true, summary: "done", blocked: nil)
        let stuck = TorStatus(percent: 10, ready: false, summary: "connecting", blocked: "clock skew")
        let starting = TorStatus(percent: 30, ready: false, summary: "connecting", blocked: nil)
        let tor = Ui(hubs: hubs, tor: true, chain: caughtUp, scan: done)
        XCTAssertEqual(chain(ui: { var c = tor; c.torStatus = ready; return c }()).map { $0.reach }, [.up, .up])
        XCTAssertEqual(chain(ui: { var c = tor; c.torStatus = stuck; return c }()).first, Link(name: "Tor", reach: .down, detail: "Stuck: clock skew"))
        XCTAssertEqual(chain(ui: { var c = tor; c.torStatus = starting; return c }()).first, Link(name: "Tor", reach: .going, detail: "Connecting (30%)"))
        XCTAssertEqual(chain(ui: { var c = tor; c.torStatus = starting; return c }()).last?.detail, "Waiting for Tor")
        XCTAssertEqual(chain(ui: { var c = tor; c.torStatus = stuck; return c }()).map { $0.reach }.max(), .down)
    }

    func test_each_hub_and_the_mailbox_relays_say_whether_they_answered_when_last_asked() {
        let ui = Ui(hubs: [hub()], chain: caughtUp, scan: scan(from: 800, next: 901, height: 900))
        func hubLink(reached: [String: Bool]) -> Link { links(ui: { var c = ui; c.hubsReached = reached; return c }()).last! }
        XCTAssertEqual(hubLink(reached: [:]).reach, .going)
        XCTAssertEqual(hubLink(reached: ["https://hub.example": true]).reach, .up)
        let down = hubLink(reached: ["https://hub.example": false])
        XCTAssertEqual(down.reach, .down)
        XCTAssertEqual(down.name, "hub.example")
        // No mailbox: no relays looked in, none said.
        XCTAssertTrue(links(ui: ui).allSatisfy { $0.name != RELAYS })
        var mail = ui
        mail.hubs = [{ var c = hub(); c.mailbox = true; return c }()]
        mail.hubsReached = ["https://hub.example": true]
        XCTAssertEqual(links(ui: mail).last?.reach, .going)
        XCTAssertEqual(links(ui: { var c = mail; c.relaysReached = true; return c }()).last, Link(name: RELAYS, reach: .up, detail: "Reached"))
        // Full of mail, junk perhaps: a payment may be hidden there.
        let full = links(ui: { var c = mail; c.relaysReached = true; c.relaysFull = true; return c }()).last!
        XCTAssertEqual(full.reach, .down)
        XCTAssertTrue(full.detail.contains("may hide a payment"), full.detail)
        XCTAssertEqual(links(ui: { var c = mail; c.relaysReached = false; return c }()).map { $0.reach }.max(), .down)
    }

    func test_turning_tor_off_is_offered_once_it_keeps_the_app_from_bitcoin() {
        let ready = TorStatus(percent: 100, ready: true, summary: "done", blocked: nil)
        let stuck = TorStatus(percent: 10, ready: false, summary: "connecting", blocked: "clock skew")
        let tor = Ui(tor: true, chain: caughtUp, torStatus: ready)
        XCTAssertFalse(torInTheWay(ui: tor))
        XCTAssertFalse(offerTorOff(ui: tor, stuckMs: nil))
        // Connected to Tor, but no Bitcoin node through it: after a while.
        let noNodes = { var c = tor; c.chain = ChainStatus(); return c }()
        XCTAssertTrue(torInTheWay(ui: noNodes))
        XCTAssertFalse(offerTorOff(ui: noNodes, stuckMs: TOR_PATIENCE_MS - 1))
        XCTAssertTrue(offerTorOff(ui: noNodes, stuckMs: TOR_PATIENCE_MS))
        // Stuck by its own word: at once.
        XCTAssertTrue(offerTorOff(ui: { var c = tor; c.torStatus = stuck; return c }(), stuckMs: nil))
        // Off, nothing to offer.
        XCTAssertFalse(offerTorOff(ui: { var c = noNodes; c.tor = false; return c }(), stuckMs: TOR_PATIENCE_MS))
        // Importing the pinned headers, no node asked yet: Tor not in the way.
        for importing in [0, 42, 100] {
            let importingNoNodes = { var c = noNodes; c.chain = ChainStatus(importing: importing); return c }()
            XCTAssertFalse(torInTheWay(ui: importingNoNodes))
            XCTAssertFalse(offerTorOff(ui: importingNoNodes, stuckMs: TOR_PATIENCE_MS))
            // Unless Tor itself is not connected.
            var notConnected = importingNoNodes
            notConnected.torStatus = { var c = stuck; c.blocked = nil; return c }()
            XCTAssertTrue(torInTheWay(ui: notConnected))
        }
        // Said as it is: Tor connected, no node through it, or Tor not getting through.
        XCTAssertTrue(torOffMeans(ui: noNodes).hasPrefix("No Bitcoin node answers through Tor."))
        XCTAssertTrue(torOffMeans(ui: { var c = tor; c.torStatus = stuck; return c }()).hasPrefix("Tor is not getting through."))
    }

    func test_no_peer_height_no_fraction() {
        let ui = Ui(hubs: [hub()], chain: ChainStatus(peers: 1, headers: 400))
        let headers = stages(ui: ui)[1]
        XCTAssertEqual(headers.detail, "Block 400")
        XCTAssertNil(headers.fraction)
    }

    func test_tor_comes_first_over_tor_and_says_why_it_is_stuck() {
        let blocked = TorStatus(percent: 10, ready: false, summary: "connecting", blocked: "clock skew")
        let ui = Ui(hubs: [hub()], tor: true, chain: caughtUp, torStatus: blocked)
        let tor = stages(ui: ui).first!
        XCTAssertEqual(tor.name, "Tor")
        XCTAssertEqual(tor.state, .doing)
        XCTAssertEqual(tor.detail, "Stuck: clock skew")
        XCTAssertEqual(tor.fraction!, 0.1, accuracy: 0.0001)
        XCTAssertTrue(stages(ui: ui).dropFirst(1).allSatisfy { $0.state == .waiting })
        XCTAssertEqual(stages(ui: { var c = ui; c.torStatus = nil; return c }()).first!.detail, "Starting")
    }

    func test_payments_are_looked_for_from_the_birthday() {
        let ui = Ui(hubs: [hub()], chain: caughtUp, scan: scan(from: 800, next: 850, height: 900))
        let payments = stages(ui: ui).last!
        XCTAssertEqual(payments.state, .doing)
        XCTAssertEqual(payments.detail, "Block 850 of 900, 51 to go (49%)")
        XCTAssertEqual(payments.fraction!, 50.0 / 101.0, accuracy: 0.0001)
    }

    func test_caught_up_is_up_to_date_and_says_when_without_tweaks() {
        let ui = Ui(hubs: [hub()], chain: caughtUp, scan: scan(from: 800, next: 901, height: 900))
        XCTAssertEqual(stages(ui: ui).last!.detail, "Up to date")
        XCTAssertFalse(catchingUp(ui: ui))
        let fallback = { var c = ui; c.scan = scan(from: 800, next: 901, height: 900, tweaks: false); return c }()
        XCTAssertEqual(stages(ui: fallback).last!.detail, "Up to date, fallback addresses only")
    }

    func test_a_hub_behind_the_light_client_is_waited_for() {
        let ui = Ui(hubs: [hub()], chain: caughtUp, scan: scan(from: 800, next: 896, height: 895))
        let payments = stages(ui: ui).last!
        XCTAssertEqual(payments.state, .doing)
        XCTAssertEqual(payments.detail, "Checked to block 895, checking block 896")
    }

    func test_caught_up_once_bitcoin_stays_up_while_keeping_up_with_new_blocks() {
        // A block come since the last look: looked in next round.
        let ui = Ui(hubs: [hub()], chain: caughtUp, scan: scan(from: 800, next: 900, height: 899))
        XCTAssertEqual(stages(ui: ui).last!.state, .doing)
        XCTAssertEqual(stages(ui: ui).last!.detail, "Checked to block 899, checking block 900")
        XCTAssertEqual(said(stage: stages(ui: ui).last!, developer: false), "Checking the newest block")
        XCTAssertTrue(bitcoinBehind(ui: ui))
        func bitcoin(ui: Ui, stale: Bool) -> Link { links(ui: ui, stale: stale).first { $0.name == BITCOIN }! }
        // Before the wallet first caught up: said at once.
        XCTAssertEqual(bitcoin(ui: ui, stale: false).reach, .going)
        // Since: up, as usual, until behind STALE_MS.
        let since = { var c = ui; c.caughtUp = true; return c }()
        XCTAssertEqual(bitcoin(ui: since, stale: false), Link(name: BITCOIN, reach: .up, detail: "Up to date"))
        XCTAssertEqual(bitcoin(ui: since, stale: true).reach, .going)
        // No node: the same, up for a while, then said.
        var noNodes = since
        noNodes.chain = { var c = caughtUp; c.peers = 0; return c }()
        XCTAssertEqual(bitcoin(ui: noNodes, stale: false).reach, .up)
        XCTAssertEqual(bitcoin(ui: noNodes, stale: true), Link(name: BITCOIN, reach: .going, detail: "Looking for Bitcoin nodes"))
        // Up to date: not behind.
        XCTAssertFalse(bitcoinBehind(ui: { var c = ui; c.scan = scan(from: 800, next: 901, height: 900); return c }()))
    }

    func test_without_a_hub_payments_wait_for_one() {
        XCTAssertEqual(stages(ui: Ui(chain: caughtUp)).last!.detail, "Choose a hub first")
    }

    func test_behind_until_the_chain_and_the_payments_are_caught_up() {
        let hubs = [hub()]
        XCTAssertTrue(behind(ui: Ui(hubs: hubs, chain: ChainStatus(importing: 0))))
        XCTAssertTrue(behind(ui: Ui(hubs: hubs, chain: { var c = caughtUp; c.synced = false; return c }())))
        XCTAssertTrue(behind(ui: Ui(hubs: hubs, chain: caughtUp)))
        XCTAssertTrue(behind(ui: Ui(hubs: hubs, chain: caughtUp, scan: scan(from: 100, next: 500, height: 900))))
        XCTAssertFalse(behind(ui: Ui(hubs: hubs, chain: caughtUp, scan: scan(from: 100, next: 901, height: 900))))
        // Waiting for her to choose a hub is not catching up.
        XCTAssertFalse(behind(ui: Ui(chain: caughtUp)))
        XCTAssertTrue(behind(ui: Ui(chain: ChainStatus())))
    }
}
