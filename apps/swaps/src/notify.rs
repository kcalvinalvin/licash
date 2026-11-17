//! The provider's notifications: its events relayed to the operator's
//! targets, and an alert when it stops.

use std::{path::Path, sync::Arc, time::Duration};

use licash_notify::{
    list, sat, sat_rate, sat_signed, short, Item, Message, Outbox, Priority, Relay, Target,
};
use licash_swaps::{Event, Notice, Outcome, Provider};
use licash_wallet::Transport;

/// The kinds of the provider's events.
pub const KINDS: [&str; 6] = ["accepted", "done", "refunded", "failed", "lost", "alert"];

/// The kinds of the events of swaps under way, notified of only if asked.
const UNDER_WAY: [&str; 1] = ["accepted"];

/// How long a stopping provider tries to say so.
const STOP_WITHIN: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Notifier {
    relay: Relay,
    name: String,
}

impl Notifier {
    /// Notifications of the events of `provider` to `targets`, keeping
    /// their places in the file `cursors`, of the `kinds` of event (if
    /// empty, all but those of swaps under way), under titles headed by
    /// `name`. Targets new to `cursors` hear of the events from now on, the
    /// alerts the provider raises as it starts included.
    pub fn new<T: Transport + 'static>(
        provider: Arc<Provider<T>>,
        name: &str,
        targets: Vec<Target>,
        cursors: &Path,
        kinds: Vec<String>,
    ) -> anyhow::Result<Self> {
        let events = Arc::new(Events {
            provider,
            name: name.to_owned(),
        });
        let last = events.last()?;
        let relay = Relay::new(events, targets, cursors)?.new_targets_after(last);
        let relay = match kinds.is_empty() {
            true => relay.except(UNDER_WAY.map(str::to_owned)),
            false => relay.only(kinds),
        };
        Ok(Self {
            relay,
            name: name.to_owned(),
        })
    }

    pub fn targets(&self) -> &[Target] {
        self.relay.targets()
    }

    /// Relay the events, for as long as the future is polled.
    pub async fn run(self) {
        self.relay.run().await;
    }

    /// Tell the targets the provider stopped, for at most a few seconds.
    pub async fn stopped(&self) {
        let message = titled(
            &self.name,
            Message::new(
                Priority::Normal,
                "Swaps stopped",
                "Swaps and payouts under way resume on restart.",
            ),
        );
        if tokio::time::timeout(STOP_WITHIN, self.relay.alert(&message))
            .await
            .is_err()
        {
            tracing::warn!("the stop was not notified in time");
        }
    }
}

/// `message`, its title headed by `name` to tell services apart.
pub fn titled(name: &str, mut message: Message) -> Message {
    message.title = format!("{name}: {}", message.title);
    message
}

struct Events<T: Transport> {
    provider: Arc<Provider<T>>,
    name: String,
}

impl<T: Transport> Events<T> {
    /// What became of the swap `event` tells the end of, as recorded now.
    fn outcome(&self, event: &Event) -> Option<Outcome> {
        let id = match event.notice()? {
            Notice::Done { id, .. }
            | Notice::Refunded { id, .. }
            | Notice::Failed { id, .. }
            | Notice::Lost { id, .. } => id,
            _ => return None,
        };
        self.provider
            .outcome(&id)
            .inspect_err(|e| tracing::warn!(%id, "the swap's record: {e}"))
            .ok()
            .flatten()
    }
}

impl<T: Transport + 'static> Outbox for Events<T> {
    fn after(&self, after: u64, limit: u32) -> anyhow::Result<Vec<Item>> {
        Ok(self
            .provider
            .events(after, limit)?
            .into_iter()
            .map(|event| Item {
                seq: event.seq,
                message: titled(&self.name, message(&event, self.outcome(&event).as_ref())),
                kind: event.kind,
            })
            .collect())
    }

    fn last(&self) -> anyhow::Result<u64> {
        Ok(self.provider.last_event()?)
    }
}

/// What an operator reads of `event`, with what became of its swap if the
/// provider recorded it.
pub fn message(event: &Event, outcome: Option<&Outcome>) -> Message {
    let record = outcome.map(|o| &o.record);
    let hub_fee = outcome.and_then(|o| o.hub_fee);
    let kind = record.map_or("Swap", |r| what(&r.to));
    let mut fields: Vec<(&str, String)> = Vec::new();
    if let Some(r) = record {
        fields.push(("Route", route(&r.from, &r.to)));
    }
    let named = |mut fields: Vec<(&str, String)>, id: &str| {
        fields.push((kind, short(id)));
        list(&fields)
    };
    match event.notice() {
        Some(Notice::Accepted {
            id,
            amount,
            fee,
            from,
            to,
        }) => {
            let what = what(&to);
            let fields = [
                ("Route", route(&from, &to)),
                ("Amount", sat(amount)),
                ("Fee", sat(fee)),
                (what, short(&id)),
            ];
            Message::new(Priority::Low, format!("{what} accepted"), list(&fields))
        }
        Some(Notice::Done { id, amount, fee }) => {
            fields.push(("Amount", sat(amount)));
            // The client's lock pays the provider in full: what it paid
            // beyond the amount, the fee and any more.
            let income = record.map_or(fee, |r| r.paid.saturating_sub(r.amount));
            if kind != "Swap" {
                // The miner or routing fees it pays are in it.
                fields.push(("Charged", sat_rate(signed(income), amount)));
            } else {
                fields.push(("Income", sat_rate(signed(income), amount)));
                if let Some(cost) = hub_fee {
                    let profit = signed(income).saturating_sub(signed(cost));
                    fields.push(("Costs", sat(cost)));
                    fields.push(("Profit", sat_rate(profit, amount)));
                }
            }
            Message::new(Priority::Low, format!("{kind} done"), named(fields, &id))
        }
        Some(Notice::Refunded { id, amount }) => {
            fields.push(("Amount", sat(amount)));
            fields.push(("Reason", "the client never claimed".to_owned()));
            if let Some(cost) = hub_fee {
                fields.push(("Costs", sat(cost)));
                fields.push(("Profit", sat_signed(-signed(cost))));
            }
            Message::new(
                Priority::Normal,
                format!("{kind} refunded"),
                named(fields, &id),
            )
        }
        Some(Notice::Failed { id, reason }) => {
            if let Some(r) = record {
                fields.push(("Amount", sat(r.amount)));
            }
            fields.push(("Reason", reason));
            Message::new(
                Priority::Normal,
                format!("{kind} failed"),
                named(fields, &id),
            )
        }
        Some(Notice::Lost { id, amount, reason }) => {
            fields.push(("Lost", sat(amount)));
            if let Some(cost) = hub_fee {
                let loss = signed(amount).saturating_add(signed(cost));
                fields.push(("Costs", sat(cost)));
                fields.push(("Profit", sat_signed(-loss)));
            }
            fields.push(("Reason", reason));
            Message::new(Priority::High, format!("{kind} lost"), named(fields, &id))
        }
        Some(Notice::Alert { message }) => Message::new(Priority::High, "Swaps alert", message),
        _ => Message::new(
            Priority::Normal,
            format!("Swaps event {}", event.kind),
            event.data.to_string(),
        ),
    }
}

/// What a swap to `to` is: a payout on Bitcoin, an invoice paid over
/// Lightning, or a swap of notes.
fn what(to: &str) -> &'static str {
    if to.starts_with("bitcoin:") {
        "Payout"
    } else if to.starts_with("lightning:") {
        "Payment"
    } else {
        "Swap"
    }
}

/// The way a swap goes: from hub `from` to hub `to`, to Bitcoin or over
/// Lightning.
fn route(from: &str, to: &str) -> String {
    let to = if let Some(network) = to.strip_prefix("bitcoin:") {
        format!("Bitcoin ({network})")
    } else if let Some(network) = to.strip_prefix("lightning:") {
        format!("Lightning ({network})")
    } else {
        crate::dashboard::short(to)
    };
    format!("{} → {to}", crate::dashboard::short(from))
}

fn signed(amount: u64) -> i64 {
    i64::try_from(amount).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use licash_swaps::{Record, Stage};

    use super::*;

    fn event(notice: &Notice) -> Event {
        Event {
            seq: 1,
            at: 0,
            kind: notice.kind().to_owned(),
            data: serde_json::to_value(notice).unwrap(),
        }
    }

    /// Swap `ab` of `amount` from hub A to `to`, its client's lock paying
    /// `paid`, the provider's lock `hub_fee`.
    fn outcome(to: &str, amount: u64, paid: u64, hub_fee: Option<u64>) -> Outcome {
        Outcome {
            record: Record {
                id: "ab".into(),
                hash: "cd".into(),
                from: "https://a.example".into(),
                to: to.into(),
                amount,
                fee: 100,
                paid,
                lock: None,
                expiry: 900,
                stage: Stage::Done,
                reason: None,
                created_at: 0,
                updated_at: 0,
            },
            hub_fee,
        }
    }

    const ONION: &str = "http://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuv.onion";

    #[test]
    fn each_kind_reads_as_a_message_and_a_loss_is_urgent() {
        let id = "ab".to_owned();
        let notices = [
            Notice::Accepted {
                id: id.clone(),
                amount: 5000,
                fee: 6,
                from: "https://a.example".into(),
                to: "https://b.example/".into(),
            },
            Notice::Done {
                id: id.clone(),
                amount: 5000,
                fee: 6,
            },
            Notice::Refunded {
                id: id.clone(),
                amount: 5000,
            },
            Notice::Failed {
                id: id.clone(),
                reason: "hub refused".into(),
            },
            Notice::Lost {
                id: id.clone(),
                amount: 5000,
                reason: "too late".into(),
            },
            Notice::Alert {
                message: "look".into(),
            },
        ];
        let kinds: Vec<&str> = notices.iter().map(Notice::kind).collect();
        assert_eq!(kinds, KINDS);
        // Of a swap the provider has no record of: what the event holds.
        let messages: Vec<Message> = notices.iter().map(|n| message(&event(n), None)).collect();
        assert_eq!(messages[0].title, "Swap accepted");
        assert_eq!(
            messages[0].body,
            "• Route: a.example → b.example\n• Amount: 5,000 sat\n• Fee: 6 sat\n• Swap: ab"
        );
        assert_eq!(
            messages[1].body,
            "• Amount: 5,000 sat\n• Income: 6 sat (0.12%)\n• Swap: ab"
        );
        assert_eq!(
            messages[2].body,
            "• Amount: 5,000 sat\n• Reason: the client never claimed\n• Swap: ab"
        );
        assert_eq!(messages[3].body, "• Reason: hub refused\n• Swap: ab");
        assert_eq!(messages[4].priority, Priority::High);
        assert_eq!(
            messages[4].body,
            "• Lost: 5,000 sat\n• Reason: too late\n• Swap: ab"
        );
        assert_eq!(messages[5].body, "look");
        let unknown = Event {
            kind: "future".into(),
            data: serde_json::json!({"kind": "future"}),
            ..event(&notices[0])
        };
        assert_eq!(message(&unknown, None).title, "Swaps event future");
    }

    #[test]
    fn a_swap_that_ended_reads_with_its_route_income_costs_and_profit() {
        let id = "ab".to_owned();
        let done = event(&Notice::Done {
            id: id.clone(),
            amount: 50_000,
            fee: 100,
        });
        // Paid 6 sat beyond its fee, less what its lock paid hub B.
        let settled = outcome(ONION, 50_000, 50_106, Some(51));
        let message = message(&done, Some(&settled));
        assert_eq!(message.title, "Swap done");
        assert_eq!(
            message.body,
            "• Route: a.example → abcdefgh….onion\n• Amount: 50,000 sat\n\
             • Income: 106 sat (0.21%)\n• Costs: 51 sat\n• Profit: 55 sat (0.11%)\n\
             • Swap: ab"
        );
        // A hub fee above the income: a loss.
        let dear = outcome(ONION, 50_000, 50_106, Some(200));
        assert!(super::message(&done, Some(&dear))
            .body
            .contains("• Profit: -94 sat (-0.19%)"));
        // Its lock's fee unknown: no costs, no profit.
        let unsettled = outcome(ONION, 50_000, 50_106, None);
        assert_eq!(
            super::message(&done, Some(&unsettled)).body,
            "• Route: a.example → abcdefgh….onion\n• Amount: 50,000 sat\n\
             • Income: 106 sat (0.21%)\n• Swap: ab"
        );

        let refunded = event(&Notice::Refunded {
            id: id.clone(),
            amount: 50_000,
        });
        let back = outcome(ONION, 50_000, 50_106, Some(5));
        assert_eq!(
            super::message(&refunded, Some(&back)).body,
            "• Route: a.example → abcdefgh….onion\n• Amount: 50,000 sat\n\
             • Reason: the client never claimed\n• Costs: 5 sat\n• Profit: -5 sat\n• Swap: ab"
        );
        let failed = event(&Notice::Failed {
            id: id.clone(),
            reason: "hub refused".into(),
        });
        assert_eq!(
            super::message(&failed, Some(&unsettled)).body,
            "• Route: a.example → abcdefgh….onion\n• Amount: 50,000 sat\n\
             • Reason: hub refused\n• Swap: ab"
        );
        let lost = event(&Notice::Lost {
            id,
            amount: 50_000,
            reason: "too late".into(),
        });
        assert_eq!(
            super::message(&lost, Some(&settled)).body,
            "• Route: a.example → abcdefgh….onion\n• Lost: 50,000 sat\n• Costs: 51 sat\n\
             • Profit: -50,051 sat\n• Reason: too late\n• Swap: ab"
        );
    }

    #[test]
    fn a_payout_reads_as_one_its_charge_with_the_miner_fee_in_it() {
        let id = "cd".repeat(32);
        let accepted = Notice::Accepted {
            id: id.clone(),
            amount: 125_000,
            fee: 1_250,
            from: "https://a.example".into(),
            to: "bitcoin:signet".into(),
        };
        let accepted = message(&event(&accepted), None);
        assert_eq!(accepted.title, "Payout accepted");
        assert_eq!(
            accepted.body,
            "• Route: a.example → Bitcoin (signet)\n• Amount: 125,000 sat\n\
             • Fee: 1,250 sat\n• Payout: cdcdcdcdcdcd…"
        );
        let done = Notice::Done {
            id,
            amount: 125_000,
            fee: 1_250,
        };
        let paid = outcome("bitcoin:signet", 125_000, 126_250, None);
        let done = message(&event(&done), Some(&paid));
        assert_eq!(done.title, "Payout done");
        assert_eq!(
            done.body,
            "• Route: a.example → Bitcoin (signet)\n• Amount: 125,000 sat\n\
             • Charged: 1,250 sat (1.0%)\n• Payout: cdcdcdcdcdcd…"
        );
    }

    #[test]
    fn an_invoice_paid_reads_as_a_payment_its_charge_with_the_routing_fees_in_it() {
        let id = "ef".repeat(32);
        let accepted = Notice::Accepted {
            id: id.clone(),
            amount: 10_000,
            fee: 52,
            from: "https://a.example".into(),
            to: "lightning:regtest".into(),
        };
        let accepted = message(&event(&accepted), None);
        assert_eq!(accepted.title, "Payment accepted");
        assert_eq!(
            accepted.body,
            "• Route: a.example → Lightning (regtest)\n• Amount: 10,000 sat\n\
             • Fee: 52 sat\n• Payment: efefefefefef…"
        );
        let paid = outcome("lightning:regtest", 10_000, 10_052, None);
        let failed = Notice::Failed {
            id,
            reason: "not paid: no route".into(),
        };
        let failed = message(&event(&failed), Some(&paid));
        assert_eq!(failed.title, "Payment failed");
        assert_eq!(
            failed.body,
            "• Route: a.example → Lightning (regtest)\n• Amount: 10,000 sat\n\
             • Reason: not paid: no route\n• Payment: efefefefefef…"
        );
    }

    #[test]
    fn the_kinds_passed_over_by_default_are_of_swaps_under_way() {
        assert!(UNDER_WAY.iter().all(|k| KINDS.contains(k)));
    }
}
