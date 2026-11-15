//! The provider's notifications: its events relayed to the operator's
//! targets, and an alert when it stops.

use std::{path::Path, sync::Arc, time::Duration};

use licash_notify::{list, sat, short, Item, Message, Outbox, Priority, Relay, Target};
use licash_swaps::{Event, Notice, Provider};
use licash_wallet::Transport;

/// The kinds of the provider's events.
pub const KINDS: [&str; 6] = ["accepted", "done", "refunded", "failed", "lost", "alert"];

/// How long a stopping provider tries to say so.
const STOP_WITHIN: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Notifier {
    relay: Relay,
    name: String,
}

impl Notifier {
    /// Notifications of the events of `provider` to `targets`, keeping
    /// their places in the file `cursors`, of the `kinds` of event (all if
    /// empty), under titles headed by `name`. Targets new to `cursors` hear
    /// of the events from now on, the alerts the provider raises as it
    /// starts included.
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
        let mut relay = Relay::new(events, targets, cursors)?.new_targets_after(last);
        if !kinds.is_empty() {
            relay = relay.only(kinds);
        }
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

impl<T: Transport + 'static> Outbox for Events<T> {
    fn after(&self, after: u64, limit: u32) -> anyhow::Result<Vec<Item>> {
        Ok(self
            .provider
            .events(after, limit)?
            .into_iter()
            .map(|event| Item {
                seq: event.seq,
                message: titled(&self.name, message(&event)),
                kind: event.kind,
            })
            .collect())
    }

    fn last(&self) -> anyhow::Result<u64> {
        Ok(self.provider.last_event()?)
    }
}

/// What an operator reads of `event`.
pub fn message(event: &Event) -> Message {
    match event.notice() {
        Some(Notice::Accepted {
            id,
            amount,
            fee,
            from,
            to,
        }) => match to.strip_prefix("bitcoin:") {
            Some(network) => Message::new(
                Priority::Low,
                "Payout accepted",
                list(&[
                    ("Amount", sat(amount)),
                    ("Fee", sat(fee)),
                    ("From", from),
                    ("To", format!("Bitcoin ({network})")),
                    ("Payout", short(&id)),
                ]),
            ),
            None => Message::new(
                Priority::Low,
                "Swap accepted",
                list(&[
                    ("Amount", sat(amount)),
                    ("Fee", sat(fee)),
                    ("From", from),
                    ("To", to),
                    ("Swap", short(&id)),
                ]),
            ),
        },
        Some(Notice::Done { id, amount, fee }) => Message::new(
            Priority::Low,
            "Swap done",
            list(&[
                ("Amount", sat(amount)),
                ("Earned", sat(fee)),
                ("Swap", short(&id)),
            ]),
        ),
        Some(Notice::Refunded { id, amount }) => Message::new(
            Priority::Normal,
            "Swap refunded",
            list(&[
                ("Amount", sat(amount)),
                ("Reason", "the client never claimed".to_owned()),
                ("Swap", short(&id)),
            ]),
        ),
        Some(Notice::Failed { id, reason }) => Message::new(
            Priority::Normal,
            "Swap failed",
            list(&[("Reason", reason), ("Swap", short(&id))]),
        ),
        Some(Notice::Lost { id, amount, reason }) => Message::new(
            Priority::High,
            "Swap lost",
            list(&[
                ("Lost", sat(amount)),
                ("Reason", reason),
                ("Swap", short(&id)),
            ]),
        ),
        Some(Notice::Alert { message }) => Message::new(Priority::High, "Swaps alert", message),
        _ => Message::new(
            Priority::Normal,
            format!("Swaps event {}", event.kind),
            event.data.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(notice: &Notice) -> Event {
        Event {
            seq: 1,
            at: 0,
            kind: notice.kind().to_owned(),
            data: serde_json::to_value(notice).unwrap(),
        }
    }

    #[test]
    fn each_kind_reads_as_a_message_and_a_loss_is_urgent() {
        let id = "ab".to_owned();
        let notices = [
            Notice::Accepted {
                id: id.clone(),
                amount: 5000,
                fee: 6,
                from: "https://a.example".into(),
                to: "https://b.example".into(),
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
        let messages: Vec<Message> = notices.iter().map(|n| message(&event(n))).collect();
        assert_eq!(messages[0].title, "Swap accepted");
        assert_eq!(
            messages[0].body,
            "• Amount: 5,000 sat\n• Fee: 6 sat\n• From: https://a.example\n\
             • To: https://b.example\n• Swap: ab"
        );
        assert_eq!(
            messages[1].body,
            "• Amount: 5,000 sat\n• Earned: 6 sat\n• Swap: ab"
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
        assert_eq!(message(&unknown).title, "Swaps event future");
    }

    #[test]
    fn a_payout_reads_as_one_its_amounts_in_sat_its_id_cut_short() {
        let payout = Notice::Accepted {
            id: "cd".repeat(32),
            amount: 125_000,
            fee: 1_250,
            from: "https://a.example".into(),
            to: "bitcoin:signet".into(),
        };
        let message = message(&event(&payout));
        assert_eq!(message.title, "Payout accepted");
        assert_eq!(
            message.body,
            "• Amount: 125,000 sat\n• Fee: 1,250 sat\n• From: https://a.example\n\
             • To: Bitcoin (signet)\n• Payout: cdcdcdcdcdcd…"
        );
    }
}
