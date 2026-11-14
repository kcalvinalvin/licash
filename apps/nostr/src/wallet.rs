//! A [`Wallet`]'s payments through mailboxes: paying a payment code, and
//! taking what its own mailbox holds.

use anyhow::bail;
use licash_api::Payment;
use licash_wallet::{Error, Paid, PaymentCode, Received, Transport, Wallet};

use crate::{mailbox, Route};

/// A payment made to a payment code.
#[derive(Debug)]
pub struct CodePaid {
    pub paid: Paid,
    pub hub: String,
    pub amount: u64,
    /// How many of the hub's relays took it for the payee's mailbox: none
    /// if it was not sent there, and must be handed over.
    pub delivered: Option<usize>,
    /// Why it was not sent to the mailbox the code names.
    pub undelivered: Option<String>,
}

/// Pay `amount`, or the amount the code asks, to the wallet of `code` in
/// notes, at a hub it names that `wallet` trusts and holds enough at, then
/// send the payment to the payee's mailbox on that hub's relays, reached
/// through `route`. Refused, saying where to pay on Bitcoin if the code
/// says, when no such hub is named.
pub async fn pay<T: Transport>(
    wallet: &Wallet<T>,
    code: &PaymentCode,
    amount: Option<u64>,
    route: impl FnOnce() -> anyhow::Result<Route>,
) -> anyhow::Result<CodePaid> {
    code.check(wallet.network())?;
    let amount = match (amount, code.amount) {
        (Some(amount), Some(asked)) if amount != asked => {
            bail!("the code asks for {asked}, not {amount}")
        }
        (Some(amount), _) | (None, Some(amount)) => amount,
        (None, None) => bail!("how much to pay: the code does not say"),
    };
    let hub = match &code.licash {
        Some(licash) => wallet.hub_to_pay(licash, amount)?,
        None => None,
    };
    let (Some(licash), Some(hub)) = (&code.licash, hub) else {
        let none = format!("no hub it names is one trusted holding {amount} and the fee");
        let onchain = match (&code.silent, &code.address) {
            (Some(silent), _) => Some(silent.to_string()),
            (None, Some(address)) => Some(address.assume_checked_ref().to_string()),
            (None, None) => None,
        };
        match onchain {
            Some(to) => bail!("{none}: pay {to} on Bitcoin"),
            None => bail!("{none}"),
        }
    };
    let paid = wallet.send(&hub, &licash.address, amount).await?;
    let relays = wallet
        .hubs()?
        .into_iter()
        .find(|(url, _)| *url == hub)
        .map(|(_, info)| info.relays)
        .unwrap_or_default();
    let (delivered, undelivered) = match licash.mailbox {
        Some(to) if !relays.is_empty() => {
            let content = serde_json::to_string(&paid.payment)?;
            let sent = async { mailbox::send(&to, &content, &relays, &route()?).await };
            match sent.await {
                Ok(relays) => (Some(relays), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }
        Some(_) => (None, Some(format!("hub {hub} names no mailbox relays"))),
        None => (None, None),
    };
    Ok(CodePaid {
        paid,
        hub,
        amount,
        delivered,
        undelivered,
    })
}

/// What [`collect`] found in the mailbox.
#[derive(Debug, Default)]
pub struct Collected {
    /// The payments taken, oldest first.
    pub received: Vec<Received>,
    /// Mail not taken, by its identifier, and why.
    pub skipped: Vec<(String, String)>,
}

/// Take the payments in `wallet`'s mailbox, on the relays its hubs name,
/// reached through `route`: each once, whenever asked again.
pub async fn collect<T: Transport>(wallet: &Wallet<T>, route: &Route) -> anyhow::Result<Collected> {
    let mut relays: Vec<String> = Vec::new();
    for (_, info) in wallet.hubs()? {
        for relay in info.relays {
            if !relays.contains(&relay) {
                relays.push(relay);
            }
        }
    }
    if relays.is_empty() {
        bail!("no hub trusted names mailbox relays: adding a hub again learns them");
    }
    let mut collected = Collected::default();
    for mail in mailbox::fetch(&wallet.mailbox_secret(), &relays, route).await? {
        let Ok(payment) = serde_json::from_str::<Payment>(&mail.content) else {
            collected.skipped.push((mail.id, "is not a payment".into()));
            continue;
        };
        match wallet.receive(&payment) {
            Ok(received) => collected.received.push(received),
            Err(Error::AlreadyReceived) => {}
            Err(e) => collected.skipped.push((mail.id, format!("not taken: {e}"))),
        }
    }
    Ok(collected)
}
