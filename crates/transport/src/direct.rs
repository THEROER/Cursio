//! This machine's addresses on a direct Wi-Fi link to a peer.
//!
//! A direct link is an ordinary IP network — a Wi-Fi Direct group between two
//! machines that both stay on their router — so a connection over it is a TCP
//! connection like any other. What sets it apart is only which local address
//! it runs from, and both ends of one connection see the same answer: each
//! end's address on it belongs to the group. That is how a path is marked
//! direct with nothing added to the wire.
//!
//! The same set keeps those addresses out of discovery. A group lives only as
//! long as the two machines are near each other; announced over mDNS, its
//! address would become the address a peer dials this machine at, and stop
//! answering the moment the group goes.

use std::net::IpAddr;
use std::sync::Mutex;

fn local() -> &'static Mutex<Vec<IpAddr>> {
    static LOCAL: Mutex<Vec<IpAddr>> = Mutex::new(Vec::new());
    &LOCAL
}

/// `addr` is this machine's address on a direct link now (`on`), or no
/// longer is.
pub fn set_local(addr: IpAddr, on: bool) {
    let mut all = local().lock().unwrap_or_else(|p| p.into_inner());
    all.retain(|a| *a != addr);
    if on {
        all.push(addr);
    }
}

/// Whether `addr` is one of this machine's addresses on a direct link.
pub fn is_local(addr: IpAddr) -> bool {
    local()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(&addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_direct_only_while_set() {
        let addr: IpAddr = "192.0.2.77".parse().unwrap();
        assert!(!is_local(addr));
        set_local(addr, true);
        set_local(addr, true);
        assert!(is_local(addr));
        set_local(addr, false);
        assert!(!is_local(addr));
    }
}
