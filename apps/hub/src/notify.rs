//! The hub's notifications: its events relayed to the operator's targets,
//! and alerts for what the events cannot hold (a halt, a stop).

use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use licash_hub::{Event, Hub, Ledger, Notice, Settled};
use licash_notify::{list, sat, short, Item, Message, Outbox, Priority, Relay, Target, Timing};

/// The kinds of the hub's events.
pub const KINDS: [&str; 9] = [
    "started",
    "released",
    "double_spend",
    "locked",
    "settled",
    "activated",
    "reorg",
    "forwarded",
    "alert",
];

/// How long a stopping hub tries to say so.
const STOP_WITHIN: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Notifier {
    events: Arc<Events>,
    relay: Relay,
    timing: Timing,
}

impl Notifier {
    /// Notifications of the hub at `db` to `targets`, keeping their places
    /// in the file `cursors`, of the `kinds` of event (all if empty), under
    /// titles headed by `name`. Open it before [`Hub::open`]: targets new to
    /// `cursors` then hear of the hub's start.
    pub fn new(
        db: &Path,
        name: &str,
        targets: Vec<Target>,
        cursors: &Path,
        kinds: Vec<String>,
    ) -> anyhow::Result<Self> {
        let events = Arc::new(Events {
            ledger: Mutex::new(Ledger::open(db)?),
            name: name.to_owned(),
        });
        let mut relay =
            Relay::new(events.clone(), targets, cursors)?.new_targets_after(events.last()?);
        if !kinds.is_empty() {
            relay = relay.only(kinds);
        }
        Ok(Self {
            events,
            relay,
            timing: Timing::default(),
        })
    }

    /// How often to look for events and for a halt, and to retry.
    pub fn timing(mut self, timing: Timing) -> Self {
        self.relay = self.relay.timing(timing);
        self.timing = timing;
        self
    }

    pub fn targets(&self) -> &[Target] {
        self.relay.targets()
    }

    /// Relay the hub's events, and alert once if it halts, for as long as
    /// the future is polled.
    pub async fn run(self, hub: Arc<Hub>) {
        let halt = async {
            while !hub.halted() {
                tokio::time::sleep(self.timing.poll).await;
            }
            tracing::error!("the hub halted: its database changed under it");
            self.relay.alert(&self.events.titled(halted())).await;
        };
        tokio::join!(self.relay.clone().run(), halt);
    }

    /// Tell the targets the hub stopped, for at most a few seconds.
    pub async fn stopped(&self) {
        let message = self.events.titled(stopped());
        if tokio::time::timeout(STOP_WITHIN, self.relay.alert(&message))
            .await
            .is_err()
        {
            tracing::warn!("the stop was not notified in time");
        }
    }
}

/// `message`, its title headed by `name` to tell hubs apart.
pub fn titled(name: &str, mut message: Message) -> Message {
    message.title = format!("{name}: {}", message.title);
    message
}

/// The hub's outbox, read beside it: a halted hub's events still go out.
struct Events {
    ledger: Mutex<Ledger>,
    name: String,
}

impl Events {
    fn titled(&self, message: Message) -> Message {
        titled(&self.name, message)
    }

    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Outbox for Events {
    fn after(&self, after: u64, limit: u32) -> anyhow::Result<Vec<Item>> {
        Ok(self
            .ledger()
            .events(after, limit)?
            .into_iter()
            .map(|event| Item {
                seq: event.seq,
                message: self.titled(message(&event)),
                kind: event.kind,
            })
            .collect())
    }

    fn last(&self) -> anyhow::Result<u64> {
        Ok(self.ledger().last_event()?)
    }
}

/// What an operator reads of `event`.
pub fn message(event: &Event) -> Message {
    match event.notice() {
        Some(Notice::Started { boot }) => Message::new(
            Priority::Low,
            "Hub started",
            list(&[("Boot", short(&boot))]),
        ),
        Some(Notice::Released {
            proposal,
            amount,
            fee,
        }) => Message::new(
            Priority::Low,
            "Spend released",
            list(&[
                ("Amount", sat(amount)),
                ("Fee", sat(fee)),
                ("Proposal", short(&proposal)),
            ]),
        ),
        Some(Notice::DoubleSpend { proposal, spent_by }) => Message::new(
            Priority::High,
            "Double spend refused",
            list(&[
                ("Proposal", short(&proposal)),
                ("Spent by", short(&spent_by)),
            ]),
        ),
        Some(Notice::Locked {
            proposal,
            amount,
            fee,
            expiry,
            own: false,
        }) => Message::new(
            Priority::Low,
            "Lock admitted",
            list(&[
                ("Amount", sat(amount)),
                ("Fee if paid", sat(fee)),
                ("Expiry", format!("block {expiry}")),
                ("Proposal", short(&proposal)),
            ]),
        ),
        Some(Notice::Locked {
            proposal,
            amount,
            expiry,
            own: true,
            ..
        }) => Message::new(
            Priority::Low,
            "Own lock admitted",
            list(&[
                ("Amount", sat(amount)),
                ("Expiry", format!("block {expiry}")),
                ("Proposal", short(&proposal)),
            ]),
        ),
        Some(Notice::Settled {
            proposal,
            amount,
            fee,
            branch,
        }) => {
            let (priority, title) = match branch {
                Settled::Pay => (Priority::Low, "Lock paid"),
                Settled::Refund => (Priority::Normal, "Lock refunded"),
            };
            Message::new(
                priority,
                title,
                list(&[
                    ("Amount", sat(amount)),
                    ("Fee", sat(fee)),
                    ("Proposal", short(&proposal)),
                ]),
            )
        }
        Some(Notice::Activated {
            proposal,
            height,
            refunded: false,
        }) => Message::new(
            Priority::High,
            "Lock activated on Bitcoin",
            list(&[
                ("Block", height.to_string()),
                ("Proposal", short(&proposal)),
            ]),
        ),
        Some(Notice::Activated {
            proposal,
            height,
            refunded: true,
        }) => Message::new(
            Priority::High,
            "Refunded lock activated",
            list(&[
                ("Block", height.to_string()),
                ("Proposal", short(&proposal)),
                ("Action", "check the hub's reserves".to_owned()),
            ]),
        ),
        Some(Notice::Reorg { height }) => Message::new(
            Priority::High,
            "Checkpoint reorganized",
            list(&[
                ("Block", height.to_string()),
                (
                    "Action",
                    "the hub follows Bitcoin no more until you act".to_owned(),
                ),
            ]),
        ),
        Some(Notice::Forwarded {
            proposal,
            amount,
            failed: None,
        }) => Message::new(
            Priority::Low,
            "Payment forwarded",
            list(&[("Amount", sat(amount)), ("Proposal", short(&proposal))]),
        ),
        Some(Notice::Forwarded {
            proposal,
            amount,
            failed: Some(reason),
        }) => Message::new(
            Priority::Normal,
            "Forward failed",
            list(&[
                ("Amount", sat(amount)),
                ("Reason", reason),
                ("Proposal", short(&proposal)),
            ]),
        ),
        Some(Notice::Alert { message }) => Message::new(Priority::High, "Hub alert", message),
        _ => Message::new(
            Priority::Normal,
            format!("Hub event {}", event.kind),
            event.data.to_string(),
        ),
    }
}

pub fn halted() -> Message {
    Message::new(
        Priority::High,
        "Hub halted",
        list(&[
            ("Cause", "its database changed while it ran"),
            ("Action", "restart it on the right database"),
        ]),
    )
}

pub fn stopped() -> Message {
    Message::new(Priority::Normal, "Hub stopped", "The hub stopped serving.")
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
    fn every_kind_of_event_reads_as_its_own_message() {
        let notices = [
            Notice::Started { boot: "b0".into() },
            Notice::Released {
                proposal: "p1".into(),
                amount: 10,
                fee: 1,
            },
            Notice::DoubleSpend {
                proposal: "p2".into(),
                spent_by: "p1".into(),
            },
            Notice::Locked {
                proposal: "p3".into(),
                amount: 8,
                fee: 1,
                expiry: 900,
                own: false,
            },
            Notice::Settled {
                proposal: "p3".into(),
                amount: 8,
                fee: 1,
                branch: Settled::Refund,
            },
            Notice::Activated {
                proposal: "p3".into(),
                height: 899,
                refunded: false,
            },
            Notice::Reorg { height: 800 },
            Notice::Forwarded {
                proposal: "p".into(),
                amount: 4,
                failed: Some("no route".into()),
            },
            Notice::Alert {
                message: "look".into(),
            },
        ];
        let kinds: Vec<_> = notices.iter().map(Notice::kind).collect();
        assert_eq!(kinds, KINDS);
        let messages: Vec<_> = notices.iter().map(|n| message(&event(n))).collect();
        assert_eq!(messages[0].title, "Hub started");
        // Wallets handle what a restart means for unlinking: no news to an operator.
        assert_eq!(messages[0].body, "• Boot: b0");
        assert_eq!(messages[1].title, "Spend released");
        assert_eq!(
            messages[1].body,
            "• Amount: 10 sat\n• Fee: 1 sat\n• Proposal: p1"
        );
        assert_eq!(messages[2].priority, Priority::High);
        assert_eq!(messages[2].body, "• Proposal: p2\n• Spent by: p1");
        assert_eq!(
            messages[3].body,
            "• Amount: 8 sat\n• Fee if paid: 1 sat\n• Expiry: block 900\n• Proposal: p3"
        );
        assert_eq!(messages[4].title, "Lock refunded");
        assert_eq!(messages[5].priority, Priority::High);
        assert_eq!(messages[5].body, "• Block: 899\n• Proposal: p3");
        assert_eq!(messages[6].priority, Priority::High);
        assert!(messages[6].body.starts_with("• Block: 800\n"));
        assert_eq!(messages[7].title, "Forward failed");
        assert!(messages[7].body.contains("• Reason: no route"));
        assert_eq!(
            (messages[8].priority, messages[8].body.as_str()),
            (Priority::High, "look")
        );
        // Each own variant reads as its own message too.
        let own = Notice::Locked {
            proposal: "p4".into(),
            amount: 2,
            fee: 0,
            expiry: 7,
            own: true,
        };
        assert_eq!(message(&event(&own)).title, "Own lock admitted");
        let paid = Notice::Settled {
            proposal: "p4".into(),
            amount: 2,
            fee: 0,
            branch: Settled::Pay,
        };
        assert_eq!(message(&event(&paid)).title, "Lock paid");
        let twice = Notice::Activated {
            proposal: "p4".into(),
            height: 6,
            refunded: true,
        };
        assert_eq!(message(&event(&twice)).title, "Refunded lock activated");
    }

    #[test]
    fn an_event_of_a_later_version_still_reads() {
        let unknown = Event {
            seq: 9,
            at: 0,
            kind: "minted".into(),
            data: serde_json::json!({"kind": "minted", "amount": 5}),
        };
        let message = message(&unknown);
        assert_eq!(message.title, "Hub event minted");
        assert_eq!(message.priority, Priority::Normal);
        assert!(message.body.contains("\"amount\":5"));
        assert_eq!(titled("north", message).title, "north: Hub event minted");
    }

    #[test]
    fn amounts_read_in_sat_with_commas() {
        let released = Notice::Released {
            proposal: "p".into(),
            amount: 1_234_567,
            fee: 1_235,
        };
        let body = message(&event(&released)).body;
        assert!(
            body.contains("1,234,567 sat") && body.contains("1,235 sat"),
            "{body}"
        );
        let failed = Notice::Forwarded {
            proposal: "p".into(),
            amount: 50_000,
            failed: Some("no route".into()),
        };
        let body = message(&event(&failed)).body;
        assert!(
            body.contains("50,000 sat") && body.contains("no route"),
            "{body}"
        );
    }

    #[test]
    fn ids_read_cut_short() {
        let started = Notice::Started {
            boot: "0123456789abcdef0123456789abcdef".into(),
        };
        assert_eq!(message(&event(&started)).body, "• Boot: 0123456789ab…");
        let twice = Notice::DoubleSpend {
            proposal: "a".repeat(64),
            spent_by: "b".repeat(64),
        };
        assert_eq!(
            message(&event(&twice)).body,
            "• Proposal: aaaaaaaaaaaa…\n• Spent by: bbbbbbbbbbbb…"
        );
    }
}
