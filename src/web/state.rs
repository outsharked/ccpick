//! What the portal serves: a catalog that can be republished, a generation that advances only
//! when a viewer would see a difference, and the subscribers to notify when it does.
use crate::catalog::Catalog;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};

/// Everything a viewer can see, hashed. Two catalogs with the same fingerprint would render
/// identically, so republishing one must not wake idle tabs.
pub fn fingerprint(catalog: &Catalog) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    catalog.warnings.hash(&mut hasher);
    for source in &catalog.sources {
        source.name.hash(&mut hasher);
        source.env.id().hash(&mut hasher);
    }
    for session in &catalog.sessions {
        session.meta.id.hash(&mut hasher);
        session.meta.title.hash(&mut hasher);
        session.meta.last_ts.hash(&mut hasher);
        session.meta.msg_count.hash(&mut hasher);
        session.meta.cwd.hash(&mut hasher);
        session.live.hash(&mut hasher);
        session.default_source.hash(&mut hasher);
    }
    hasher.finish()
}

pub struct Portal {
    catalog: RwLock<Arc<Catalog>>,
    generation: AtomicU64,
    fingerprint: AtomicU64,
    subscribers: Mutex<Vec<Sender<u64>>>,
    token: String,
}

impl Portal {
    pub fn new(catalog: Catalog, token: String) -> Portal {
        let fingerprint = fingerprint(&catalog);
        Portal {
            catalog: RwLock::new(Arc::new(catalog)),
            generation: AtomicU64::new(0),
            fingerprint: AtomicU64::new(fingerprint),
            subscribers: Mutex::new(Vec::new()),
            token,
        }
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().unwrap().clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// A receiver that yields the new generation each time one is published.
    pub fn subscribe(&self) -> Receiver<u64> {
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Swaps in a new catalog. Returns whether it differed from the last one; only then does
    /// the generation advance and subscribers hear about it.
    pub fn publish(&self, catalog: Catalog) -> bool {
        let next = fingerprint(&catalog);
        *self.catalog.write().unwrap() = Arc::new(catalog);
        if next == self.fingerprint.swap(next, Ordering::Relaxed) {
            return false;
        }
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        // A closed receiver means that tab is gone; drop it rather than failing the publish.
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| tx.send(generation).is_ok());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake_catalog;

    #[test]
    fn publishing_an_identical_catalog_does_not_advance_the_generation() {
        let portal = Portal::new(fake_catalog(), "token".into());
        let before = portal.generation();
        assert!(!portal.publish(fake_catalog()), "nothing changed");
        assert_eq!(portal.generation(), before);
    }

    #[test]
    fn publishing_a_changed_catalog_advances_the_generation_and_notifies() {
        let portal = Portal::new(fake_catalog(), "token".into());
        let events = portal.subscribe();
        let mut changed = fake_catalog();
        changed.sessions[0].meta.title = "Renamed".into();
        assert!(portal.publish(changed));
        assert_eq!(portal.generation(), 1);
        assert_eq!(events.recv().unwrap(), 1);
        assert_eq!(portal.catalog().sessions[0].meta.title, "Renamed");
    }

    #[test]
    fn a_dropped_subscriber_is_forgotten_rather_than_failing_a_publish() {
        let portal = Portal::new(fake_catalog(), "token".into());
        drop(portal.subscribe());
        let mut changed = fake_catalog();
        changed.sessions[0].meta.title = "Renamed".into();
        assert!(portal.publish(changed));
    }

    #[test]
    fn the_fingerprint_covers_what_a_viewer_can_see() {
        let base = fingerprint(&fake_catalog());
        let mut other = fake_catalog();
        other.sessions[0].meta.title = "Renamed".into();
        assert_ne!(base, fingerprint(&other));
        let mut live = fake_catalog();
        live.sessions[0].live = Some((4242, 0));
        assert_ne!(base, fingerprint(&live));
        assert_eq!(base, fingerprint(&fake_catalog()));
    }
}
