import XCTest
import LicashCore
@testable import Licash

final class StatsFiguresTest: XCTestCase {
    private let utc = TimeZone(identifier: "UTC")!
    private let madrid = TimeZone(identifier: "Europe/Madrid")!

    private func at(zone: TimeZone, y: Int, m: Int, d: Int, h: Int = 0) -> Int64 {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = zone
        return Int64(calendar.date(from: DateComponents(year: y, month: m, day: d, hour: h))!.timeIntervalSince1970)
    }

    private func event(
        flow: LicashCore.Flow,
        way: LicashCore.Way = .licash,
        amount: UInt64 = 1_000,
        fee: UInt64 = 0,
        state: LicashCore.State = .done,
        time: Int64? = 100,
        took: UInt64? = nil,
        hub: UInt64? = nil,
        provider: UInt64? = nil,
        network: UInt64? = nil
    ) -> StatEvent {
        StatEvent(
            id: "e", flow: flow, way: way, state: state, at: time, took: took,
            amount: amount, fee: fee, hubFee: hub, providerFee: provider, networkFee: network
        )
    }

    func test_a_week_starts_on_the_first_day_and_a_month_on_the_first() {
        // Wednesday Nov 19, 2025.
        let now = at(zone: utc, y: 2025, m: 11, d: 19, h: 15)
        XCTAssertEqual(
            span(period: .week, now: now, back: 0, zone: utc, firstDay: .monday),
            Span(start: at(zone: utc, y: 2025, m: 11, d: 17), end: at(zone: utc, y: 2025, m: 11, d: 24))
        )
        XCTAssertEqual(
            span(period: .week, now: now, back: 0, zone: utc, firstDay: .sunday),
            Span(start: at(zone: utc, y: 2025, m: 11, d: 16), end: at(zone: utc, y: 2025, m: 11, d: 23))
        )
        XCTAssertEqual(
            span(period: .week, now: now, back: 1, zone: utc, firstDay: .monday),
            Span(start: at(zone: utc, y: 2025, m: 11, d: 10), end: at(zone: utc, y: 2025, m: 11, d: 17))
        )
        XCTAssertEqual(
            span(period: .month, now: now, back: 0, zone: utc),
            Span(start: at(zone: utc, y: 2025, m: 11, d: 1), end: at(zone: utc, y: 2025, m: 12, d: 1))
        )
        XCTAssertEqual(
            span(period: .year, now: now, back: 0, zone: utc),
            Span(start: at(zone: utc, y: 2025, m: 1, d: 1), end: at(zone: utc, y: 2026, m: 1, d: 1))
        )
        XCTAssertEqual(
            span(period: .year, now: now, back: 1, zone: utc),
            Span(start: at(zone: utc, y: 2024, m: 1, d: 1), end: at(zone: utc, y: 2025, m: 1, d: 1))
        )
        XCTAssertEqual(
            span(period: .day, now: now, back: 0, zone: utc),
            Span(start: at(zone: utc, y: 2025, m: 11, d: 19), end: at(zone: utc, y: 2025, m: 11, d: 20))
        )
        XCTAssertEqual(span(period: .all, now: now, back: 0, zone: utc), EVER)
    }

    func test_a_month_goes_back_over_a_year_and_a_day_over_a_month() {
        let now = at(zone: utc, y: 2026, m: 1, d: 31, h: 12)
        XCTAssertEqual(
            span(period: .month, now: now, back: 1, zone: utc),
            Span(start: at(zone: utc, y: 2025, m: 12, d: 1), end: at(zone: utc, y: 2026, m: 1, d: 1))
        )
        XCTAssertEqual(
            span(period: .month, now: now, back: 12, zone: utc),
            Span(start: at(zone: utc, y: 2025, m: 1, d: 1), end: at(zone: utc, y: 2025, m: 2, d: 1))
        )
        XCTAssertEqual(
            span(period: .day, now: now, back: 31, zone: utc),
            Span(start: at(zone: utc, y: 2025, m: 12, d: 31), end: at(zone: utc, y: 2026, m: 1, d: 1))
        )
    }

    func test_a_day_is_the_users_own_and_is_23_hours_long_when_the_clocks_go_forward() {
        // Madrid, Sunday March 29, 2026: 02:00 became 03:00.
        let now = at(zone: madrid, y: 2026, m: 3, d: 29, h: 12)
        let day = span(period: .day, now: now, back: 0, zone: madrid)
        XCTAssertEqual(day.start, at(zone: madrid, y: 2026, m: 3, d: 29))
        XCTAssertEqual(day.end - day.start, 23 * 3600)
        XCTAssertTrue(day.contains(at: now))
        XCTAssertTrue(!day.contains(at: day.end))
        XCTAssertEqual(
            span(period: .month, now: now, back: 0, zone: madrid),
            Span(start: at(zone: madrid, y: 2026, m: 3, d: 1), end: at(zone: madrid, y: 2026, m: 4, d: 1))
        )
    }

    func test_a_period_is_called_what_it_is_near_to_now() {
        let now = at(zone: utc, y: 2025, m: 11, d: 19, h: 15)
        let us = Locale(identifier: "en_US")
        let say = { (p: Period, back: Int) in label(period: p, now: now, back: back, zone: self.utc, locale: us) }
        XCTAssertEqual(say(.day, 0), "Today")
        XCTAssertEqual(say(.day, 1), "Yesterday")
        XCTAssertEqual(say(.day, 7), "Nov 12")
        XCTAssertEqual(label(period: .day, now: now, back: 395, zone: utc, locale: us), "Oct 20, 2024")
        XCTAssertEqual(say(.week, 0), "This week")
        XCTAssertEqual(say(.week, 1), "Last week")
        XCTAssertEqual(say(.week, 2), "Nov 3 – 9")
        XCTAssertEqual(say(.week, 3), "Oct 27 – Nov 2")
        XCTAssertEqual(say(.month, 0), "This month")
        XCTAssertEqual(say(.month, 2), "September")
        XCTAssertEqual(say(.month, 11), "December 2024")
        XCTAssertEqual(say(.year, 0), "This year")
        XCTAssertEqual(say(.year, 2), "2023")
        XCTAssertEqual(say(.all, 0), "All time")
    }

    func test_the_usual_is_the_middle_and_the_average_is_the_mean() throws {
        XCTAssertEqual(median(values: [9, 1, 5]), 5)
        XCTAssertEqual(median(values: [1, 4, 6, 100]), 5)
        // Three sends of 1 sat on 1,000, and one stuck at 1,000.
        let sends = [UInt64(1), 1, 1, 1_000].map { event(flow: .sent, fee: $0, took: $0 == 1_000 ? 600 : 2) }
        let c = try XCTUnwrap(costs(events: sends))
        XCTAssertEqual(c.count, 4)
        XCTAssertEqual(c.usualFee, 1)
        XCTAssertEqual(c.meanFee, 250)
        XCTAssertEqual(c.usualPpm, 1_000)
        XCTAssertEqual(c.meanPpm, 250_750)
        XCTAssertEqual(c.usualSecs, 2)
        XCTAssertEqual(c.meanSecs, 151)
        XCTAssertNil(costs(events: []))
    }

    func test_what_has_no_amount_has_no_share_and_what_was_not_timed_has_no_time() throws {
        let c = try XCTUnwrap(costs(events: [event(flow: .optimized, amount: 0, fee: 1)]))
        XCTAssertEqual(c.usualFee, 1)
        XCTAssertNil(c.usualPpm)
        XCTAssertNil(c.usualSecs)
        XCTAssertEqual(cost(fee: c.usualFee, ppm: c.usualPpm), "1 sat")
        XCTAssertEqual(cost(fee: 0, ppm: nil), "None")
        XCTAssertEqual(cost(fee: 12, ppm: 2_400), "12 sat · 0.24%")
    }

    func test_a_period_counts_what_was_done_in_it_by_way_and_leaves_out_the_rest() {
        let events = [
            event(flow: .received, way: .licash, amount: 500, time: 100),
            event(flow: .received, way: .licash, amount: 700, time: 200),
            event(flow: .received, way: .bitcoin, amount: 3_000, time: 150),
            event(flow: .received, way: .lightning, amount: 90, time: 5_000), // another period
            event(flow: .sent, way: .lightning, amount: 200, fee: 2, time: 120, hub: 2),
            event(flow: .sent, way: .lightning, amount: 300, fee: 0, state: .failed, time: 130),
            event(flow: .sent, way: .licash, amount: 400, fee: 0, state: .pending, time: 140)
        ]
        let s = summarize(events: events, unlinks: [], span: Span(start: 0, end: 1_000))
        XCTAssertEqual(s.received[.licash], Tally(count: 2, amount: 1_200))
        XCTAssertEqual(s.received[.bitcoin], Tally(count: 1, amount: 3_000))
        XCTAssertNil(s.received[.lightning])
        XCTAssertEqual(s.sent[.lightning], Tally(count: 1, amount: 200))
        XCTAssertNil(s.sent[.licash])
        XCTAssertEqual(s.failed, 1)
        XCTAssertEqual(s.events, 6)
        XCTAssertEqual(s.fees.total, 2)
        // All time has all of it.
        XCTAssertEqual(summarize(events: events, unlinks: [], span: EVER).events, 7)
    }

    func test_a_coin_whose_block_time_is_not_known_counts_in_all_time_only() {
        let events = [event(flow: .received, way: .bitcoin, amount: 3_000, time: nil)]
        XCTAssertEqual(summarize(events: events, unlinks: [], span: Span(start: 0, end: 1_000)).events, 0)
        XCTAssertEqual(summarize(events: events, unlinks: [], span: EVER).received[.bitcoin], Tally(count: 1, amount: 3_000))
    }

    func test_fees_are_split_by_purpose_and_by_whom_and_what_is_not_itemized_is_said() {
        let events = [
            event(flow: .sent, way: .bitcoin, fee: 100, hub: 1, provider: 4, network: 95),
            event(flow: .sent, way: .lightning, fee: 2, hub: 2),
            event(flow: .moved, way: .licash, fee: 5, hub: 1, provider: 4),
            event(flow: .moved, way: .bitcoin, fee: 100, hub: 100),
            event(flow: .optimized, amount: 0, fee: 1, hub: 1),
            event(flow: .renewed, fee: 3, hub: 3),
            event(flow: .normalized, fee: 1, hub: 1),
            // A contract's price: no part of it is told.
            event(flow: .protection, way: .bitcoin, amount: 10_000, fee: 50),
            // Nothing paid for what failed or came.
            event(flow: .sent, fee: 0, state: .failed),
            event(flow: .received, fee: 0)
        ]
        let f = fees(events: events)
        XCTAssertEqual(f.total, 262)
        XCTAssertEqual(f.sending, [.bitcoin: 100, .lightning: 2])
        XCTAssertEqual(f.moving, 105)
        XCTAssertEqual(f.upkeep, 5)
        XCTAssertEqual(f.protection, 50)
        XCTAssertEqual(f.hub, 109)
        XCTAssertEqual(f.provider, 8)
        XCTAssertEqual(f.network, 95)
        XCTAssertEqual(f.unsplit, 50)
        XCTAssertEqual(f.hub + f.provider + f.network + f.unsplit, f.total)
    }

    func test_each_way_of_paying_has_its_costs_and_so_has_note_upkeep_and_protection() throws {
        let events = [
            event(flow: .sent, way: .licash, amount: 1_000, fee: 1, took: 1),
            event(flow: .sent, way: .lightning, amount: 1_000, fee: 5, took: 4),
            event(flow: .sent, way: .lightning, amount: 1_000, fee: 7, took: 8),
            event(flow: .optimized, amount: 0, fee: 1, took: 2),
            event(flow: .protection, way: .bitcoin, amount: 100_000, fee: 500)
        ]
        let s = summarize(events: events, unlinks: [], span: EVER)
        XCTAssertEqual(try XCTUnwrap(s.sending[.licash]).usualFee, 1)
        XCTAssertEqual(try XCTUnwrap(s.sending[.lightning]).usualFee, 6)
        XCTAssertEqual(try XCTUnwrap(s.sending[.lightning]).usualSecs, 6)
        XCTAssertNil(s.sending[.bitcoin])
        XCTAssertEqual(try XCTUnwrap(s.optimizing).usualSecs, 2)
        XCTAssertEqual(s.covered, Tally(count: 1, amount: 100_000))
        let protecting = try XCTUnwrap(s.protecting)
        XCTAssertEqual(protecting.usualFee, 500)
        XCTAssertEqual(protecting.usualPpm, 5_000)
        XCTAssertEqual(ppmShare(ppm: protecting.usualPpm), "0.50%")
    }

    func test_notes_unlinked_in_a_period_are_timed_by_their_wait() throws {
        let unlinks = [
            Unlink(at: 100, waited: 60, amount: 1),
            Unlink(at: 200, waited: 3_600, amount: 1),
            Unlink(at: 300, waited: 120, amount: 1),
            Unlink(at: 9_000, waited: 99, amount: 1)
        ]
        let s = summarize(events: [], unlinks: unlinks, span: Span(start: 0, end: 1_000))
        let unlinking = try XCTUnwrap(s.unlinking)
        XCTAssertEqual(unlinking.first, 3)
        XCTAssertEqual(unlinking.second.first, 120)
        XCTAssertEqual(unlinking.second.second, 1_260)
        XCTAssertNil(summarize(events: [], unlinks: [], span: EVER).unlinking)
    }

    func test_a_time_is_said_as_short_as_it_can_be() {
        XCTAssertEqual(took(seconds: 0), "under a second")
        XCTAssertEqual(took(seconds: 4), "4 s")
        XCTAssertEqual(took(seconds: 89), "89 s")
        XCTAssertEqual(took(seconds: 100), "2 min")
        XCTAssertEqual(took(seconds: 2_700), "45 min")
        XCTAssertEqual(took(seconds: 3 * 3_600), "3 h")
        XCTAssertEqual(took(seconds: 2 * 86_400), "2 days")
    }

    func test_the_balance_is_read_from_the_start_of_a_period_to_its_end_or_now() {
        let span = Span(start: 1_000, end: 2_000)
        XCTAssertEqual(instants(span: span, now: 5_000, first: nil, steps: 4), (0...4).map { 1_000 + Int64($0) * 250 })
        // A period still going is read up to now.
        XCTAssertEqual(instants(span: span, now: 1_500, first: nil, steps: 1), [1_000, 1_500])
        // One not begun is read once.
        XCTAssertEqual(instants(span: span, now: 500, first: nil), [500])
        // A span that began before the first thing that happened starts there, less a second.
        XCTAssertEqual(instants(span: span, now: 1_500, first: 500, steps: 1), [1_000, 1_500])
        var fromZero = span
        fromZero.start = 0
        XCTAssertEqual(instants(span: fromZero, now: 1_000, first: 500, steps: 1), [499, 1_000])
        // All starts at the first thing that happened.
        XCTAssertEqual(instants(span: EVER, now: 600, first: 100, steps: 1), [100, 600])
        XCTAssertEqual(instants(span: EVER, now: 600, first: nil), [600])
    }

    func test_the_balance_changes_from_its_first_point_to_its_last() throws {
        let point = { (at: Int64, licash: UInt64, bitcoin: UInt64) in BalancePoint(at: at, licash: licash, bitcoin: bitcoin, hubs: []) }
        let c = try XCTUnwrap(change(points: [point(0, 100, 50), point(1, 500, 0), point(2, 90, 10)]))
        XCTAssertEqual(c, Change(start: 150, end: 100))
        XCTAssertEqual(c.up, false)
        XCTAssertEqual(c.by, 50)
        XCTAssertNil(change(points: []))
    }
}

final class AveragedTest: XCTestCase {
    private func held(notes: UInt64, covered: UInt64, onchain: UInt64 = 0) -> Balance {
        Balance(notes: notes, notesPending: 0, onchain: onchain, moving: 0, kept: 0, opening: 0, covered: covered)
    }

    private func sample(at: Int64, notes: UInt64, covered: UInt64, onchain: UInt64 = 0) -> BalanceSample {
        BalanceSample(at: at, balance: held(notes: notes, covered: covered, onchain: onchain))
    }

    private let hour: Int64 = 3_600

    func test_each_sample_holds_until_the_next_and_the_average_weighs_by_time() throws {
        // 100 held, 100 protected for 1 h. Then 100 held, none protected for 3 h. Read at hour 4.
        let samples = [sample(at: 0, notes: 100, covered: 100), sample(at: hour, notes: 100, covered: 0)]
        let a = try XCTUnwrap(averaged(samples: samples, span: EVER, now: 4 * hour))
        XCTAssertEqual(a.protected, 25)
        XCTAssertEqual(a.unprotected, 75)
        XCTAssertEqual(a.ppm, 250_000)
        XCTAssertEqual(a.recorded, 4 * hour)
        XCTAssertEqual(a.spanSecs, 4 * hour)
        XCTAssertEqual(a.since, 0)
    }

    func test_the_share_is_all_protected_over_all_held_not_the_mean_of_shares() throws {
        // An hour with 1,000 held, all protected. An hour with 10 held, none.
        let samples = [sample(at: 0, notes: 1_000, covered: 1_000), sample(at: hour, notes: 10, covered: 0)]
        let a = try XCTUnwrap(averaged(samples: samples, span: EVER, now: 2 * hour))
        XCTAssertEqual(a.ppm, 990_099)
    }

    func test_a_span_is_clipped_and_only_what_was_recorded_of_it_counts() throws {
        let samples = [sample(at: 10 * hour, notes: 100, covered: 100)]
        // The span began before the first sample: only from it, said so.
        let a = try XCTUnwrap(averaged(samples: samples, span: Span(start: 0, end: 20 * hour), now: 14 * hour))
        XCTAssertEqual(a.recorded, 4 * hour)
        XCTAssertEqual(a.since, 10 * hour)
        // Of the span so far, from its start: 4 of 14 hours.
        XCTAssertEqual(a.spanSecs, 14 * hour)
        // One entirely before it, or not yet begun: nothing.
        XCTAssertNil(averaged(samples: samples, span: Span(start: 0, end: 5 * hour), now: 14 * hour))
        XCTAssertNil(averaged(samples: samples, span: Span(start: 30 * hour, end: 40 * hour), now: 14 * hour))
        XCTAssertNil(averaged(samples: [], span: EVER, now: 14 * hour))
    }

    func test_a_gap_beyond_a_day_and_a_half_with_a_changed_balance_is_left_out() throws {
        let gap = 48 * hour
        let samples = [sample(at: 0, notes: 100, covered: 100), sample(at: gap, notes: 100, covered: 0)]
        // Hours 0 to 48 are unknown. The last holds from 48 to 50.
        let a = try XCTUnwrap(averaged(samples: samples, span: EVER, now: gap + 2 * hour))
        XCTAssertEqual(a.recorded, 2 * hour)
        XCTAssertEqual(a.protected, 0)
        // The same balance at both ends held across it.
        let same = [sample(at: 0, notes: 100, covered: 100), sample(at: gap, notes: 100, covered: 100)]
        let b = try XCTUnwrap(averaged(samples: same, span: EVER, now: gap + 2 * hour))
        XCTAssertEqual(b.recorded, gap + 2 * hour)
        XCTAssertEqual(b.ppm, 1_000_000)
        // The last one, old, holds for no time at all.
        XCTAssertNil(averaged(samples: [sample(at: 0, notes: 100, covered: 100)], span: EVER, now: 40 * hour))
    }

    func test_what_is_on_bitcoin_is_protected_as_home_says_and_nothing_held_has_no_share() throws {
        // Bitcoin held is the wallet's own: all protected, as Home counts it.
        let a = try XCTUnwrap(averaged(samples: [sample(at: 0, notes: 100, covered: 0, onchain: 100)], span: EVER, now: hour))
        XCTAssertEqual(a.protected, 100)
        XCTAssertEqual(a.unprotected, 100)
        let none = try XCTUnwrap(averaged(samples: [sample(at: 0, notes: 0, covered: 0)], span: EVER, now: hour))
        XCTAssertEqual(none.ppm, 0)
        XCTAssertEqual(none.recorded, hour)
    }
}
