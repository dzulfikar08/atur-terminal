//! Which address a connection used, and the server's addresses the app keeps for next time.

use muxy_client::{Client, Route};
use muxy_protocol::MAX_PAIRING_HOSTS;
use serde_json::{Value, json};

/// The address a connection was made through: what the app shows as "Local · 3 ms".
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RouteInfo {
    pub(crate) host: String,
    /// `local`, `tailscale` or `other`.
    pub(crate) kind: &'static str,
    /// Connect, TLS handshake and sign-in of the winning address, in milliseconds.
    pub(crate) ms: f64,
}

impl RouteInfo {
    pub(crate) fn new(route: &Route) -> Self {
        Self {
            host: route.host.clone(),
            kind: route.kind.name(),
            ms: route.elapsed.as_secs_f64() * 1000.0,
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({ "host": self.host, "kind": self.kind, "ms": self.ms })
    }
}

/// The addresses to keep for a server after a connection through `winner`: the ones the server
/// just reported first (the newest knowledge), then `winner` if the server did not list it, then
/// the stored ones that are not in the list yet. Each address once (names ignore case), at most
/// as many as a pairing link holds, and `winner` never lost to the cap.
pub(crate) fn merge_hosts(learned: &[String], winner: &str, stored: &[String]) -> Vec<String> {
    let reserve = usize::from(!learned.iter().any(|host| host.eq_ignore_ascii_case(winner)));
    let mut merged: Vec<String> = Vec::with_capacity(MAX_PAIRING_HOSTS);
    let mut add = |host: &str, limit: usize| {
        let host = host.trim();
        if !host.is_empty()
            && merged.len() < limit
            && !merged.iter().any(|known| known.eq_ignore_ascii_case(host))
        {
            merged.push(host.to_owned());
        }
    };
    for host in learned {
        add(host, MAX_PAIRING_HOSTS - reserve);
    }
    add(winner, MAX_PAIRING_HOSTS);
    for host in stored {
        add(host, MAX_PAIRING_HOSTS);
    }
    merged
}

/// Asks a server that can tell for its current addresses and merges them with the `stored` ones.
/// A server that cannot tell, or does not answer, leaves the stored addresses as they were.
pub(crate) fn learn(client: &Client, stored: &[String], winner: &str) -> Vec<String> {
    let learned = if client.supports_addresses() {
        client
            .request_addresses()
            .map(|addresses| addresses.hosts)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    merge_hosts(&learned, winner, stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(hosts: &[&str]) -> Vec<String> {
        hosts.iter().map(|host| (*host).to_owned()).collect()
    }

    #[test]
    fn learned_addresses_come_first_then_the_stored_ones_not_in_them() {
        let merged = merge_hosts(
            &list(&["10.184.172.228", "100.64.0.9"]),
            "10.184.172.228",
            &list(&["192.168.1.5", "100.64.0.9", "mac.local"]),
        );
        assert_eq!(
            merged,
            ["10.184.172.228", "100.64.0.9", "192.168.1.5", "mac.local"]
        );
    }

    #[test]
    fn nothing_learned_keeps_the_stored_addresses_with_the_winner_among_them() {
        let stored = list(&["192.168.1.5", "mac.local"]);
        assert_eq!(
            merge_hosts(&[], "mac.local", &stored),
            ["mac.local", "192.168.1.5"]
        );
        assert_eq!(
            merge_hosts(&[], "192.168.1.5", &stored),
            ["192.168.1.5", "mac.local"]
        );
    }

    #[test]
    fn names_ignore_case_and_blanks_are_dropped() {
        let merged = merge_hosts(
            &list(&["Mac.local", " "]),
            "mac.LOCAL",
            &list(&["MAC.local", "10.0.0.2"]),
        );
        assert_eq!(merged, ["Mac.local", "10.0.0.2"]);
    }

    #[test]
    fn at_most_eight_and_the_winner_is_never_lost() {
        let learned: Vec<String> = (1..=8).map(|last| format!("10.0.0.{last}")).collect();
        let merged = merge_hosts(&learned, "100.64.0.1", &list(&["192.168.1.5"]));
        assert_eq!(merged.len(), 8);
        assert_eq!(merged[7], "100.64.0.1");
        assert_eq!(&merged[..7], &learned[..7]);
        // Listed by the server already: nothing is reserved for it.
        let merged = merge_hosts(&learned, "10.0.0.8", &[]);
        assert_eq!(merged, learned);
    }
}
