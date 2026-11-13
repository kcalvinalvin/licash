//! The hub's notifications: its events relayed to the operator's targets,
//! and alerts for what the events cannot hold (a halt, a stop).

use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use licash_hub::{Event, Hub, Ledger, Notice};
use licash_notify::{Item, Message, Outbox, Priority, Relay, Target, Timing};

/// The kinds of the hub's events.
pub const KINDS: [&str; 3] = ["started", "released", "double_spend"];

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
            format!(
                "Boot {boot}. Notes issued before this start can no longer be unlinked: \
                 their holders swap them."
            ),
        ),
        Some(Notice::Released {
            proposal,
            amount,
            fee,
        }) => Message::new(
            Priority::Low,
            "Spend released",
            format!("{amount} moved, {fee} of it to the hub. Proposal {proposal}."),
        ),
        Some(Notice::DoubleSpend { proposal, spent_by }) => Message::new(
            Priority::High,
            "Double spend refused",
            format!("Proposal {proposal} spends inputs that {spent_by} spent."),
        ),
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
        "The database changed under the running hub, which serves nothing more. \
         Restart it once the database is the one it should be.",
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
        ];
        let kinds: Vec<_> = notices.iter().map(Notice::kind).collect();
        assert_eq!(kinds, KINDS);
        let messages: Vec<_> = notices.iter().map(|n| message(&event(n))).collect();
        assert_eq!(messages[0].title, "Hub started");
        assert!(messages[0].body.contains("b0"));
        assert_eq!(messages[1].title, "Spend released");
        assert_eq!(
            messages[1].body,
            "10 moved, 1 of it to the hub. Proposal p1."
        );
        assert_eq!(messages[2].priority, Priority::High);
        assert!(messages[2].body.contains("p2") && messages[2].body.contains("p1"));
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
}
