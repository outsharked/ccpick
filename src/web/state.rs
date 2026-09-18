//! What the portal serves: a catalog that can be republished, a generation that advances only
//! when a viewer would see a difference, and the subscribers to notify when it does.
use crate::catalog::Catalog;
use std::hash::{Hash, Hasher};
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
        session.meta.branch.hash(&mut hasher);
        session.meta.last_ts.hash(&mut hasher);
        session.meta.msg_count.hash(&mut hasher);
        session.meta.cwd.hash(&mut hasher);
        session.live.hash(&mut hasher);
        session.default_source.hash(&mut hasher);
    }
    hasher.finish()
}

/// The catalog currently being served, plus the fingerprint and generation it was published
/// with. Kept behind one lock so a publish can never be observed half-applied: `catalog()` and
/// `generation()` always see a fingerprint that matches the catalog it was computed from.
struct Published {
    catalog: Arc<Catalog>,
    fingerprint: u64,
    generation: u64,
}

pub struct Portal {
    published: RwLock<Published>,
    subscribers: Mutex<Vec<Sender<u64>>>,
    token: String,
}

impl Portal {
    pub fn new(catalog: Catalog, token: String) -> Portal {
        let fingerprint = fingerprint(&catalog);
        Portal {
            published: RwLock::new(Published {
                catalog: Arc::new(catalog),
                fingerprint,
                generation: 0,
            }),
            subscribers: Mutex::new(Vec::new()),
            token,
        }
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.published.read().unwrap().catalog.clone()
    }

    pub fn generation(&self) -> u64 {
        self.published.read().unwrap().generation
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
        let mut guard = self.published.write().unwrap();
        let changed = next != guard.fingerprint;
        // Replace under the lock so catalog/fingerprint/generation move together as one atomic
        // step, but drop the superseded catalog after releasing it: dropping a Catalog can be
        // slow (it owns the whole session list), and readers shouldn't wait on that.
        let old = std::mem::replace(&mut guard.catalog, Arc::new(catalog));
        guard.fingerprint = next;
        if changed {
            guard.generation += 1;
        }
        let generation = guard.generation;
        drop(guard);
        drop(old);
        if !changed {
            return false;
        }
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
        let survivor = portal.subscribe();
        let doomed = portal.subscribe();
        drop(doomed);

        let mut first = fake_catalog();
        first.sessions[0].meta.title = "Renamed once".into();
        assert!(portal.publish(first));

        let mut second = fake_catalog();
        second.sessions[0].meta.title = "Renamed twice".into();
        assert!(portal.publish(second));

        // The survivor heard both publishes...
        assert_eq!(survivor.recv().unwrap(), 1);
        assert_eq!(survivor.recv().unwrap(), 2);
        // ...and the dropped one was pruned rather than left to accumulate forever.
        assert_eq!(
            portal.subscribers.lock().unwrap().len(),
            1,
            "the dropped subscriber should have been forgotten"
        );
    }

    #[test]
    fn the_fingerprint_covers_what_a_viewer_can_see() {
        let base = fingerprint(&fake_catalog());

        let mut title = fake_catalog();
        title.sessions[0].meta.title = "Renamed".into();
        assert_ne!(base, fingerprint(&title), "title");

        let mut branch = fake_catalog();
        branch.sessions[0].meta.branch = Some("feature/x".into());
        assert_ne!(base, fingerprint(&branch), "branch");

        let mut last_ts = fake_catalog();
        last_ts.sessions[0].meta.last_ts = Some(123456);
        assert_ne!(base, fingerprint(&last_ts), "last_ts");

        let mut msg_count = fake_catalog();
        msg_count.sessions[0].meta.msg_count += 1;
        assert_ne!(base, fingerprint(&msg_count), "msg_count");

        let mut cwd = fake_catalog();
        cwd.sessions[0].meta.cwd = Some("/somewhere/else".into());
        assert_ne!(base, fingerprint(&cwd), "cwd");

        let mut live = fake_catalog();
        live.sessions[0].live = Some((4242, 0));
        assert_ne!(base, fingerprint(&live), "live");

        let mut default_source = fake_catalog();
        default_source.sessions[0].default_source += 1;
        assert_ne!(base, fingerprint(&default_source), "default_source");

        let mut warnings = fake_catalog();
        warnings.warnings.push("something went wrong".into());
        assert_ne!(base, fingerprint(&warnings), "warnings");

        let mut source_name = fake_catalog();
        source_name.sources[0].name = "renamed source".into();
        assert_ne!(base, fingerprint(&source_name), "source name");

        // first_prompt is never shown to a viewer, so it must not be in the payload.
        let mut first_prompt = fake_catalog();
        first_prompt.sessions[0].meta.first_prompt = "different prompt entirely".into();
        assert_eq!(base, fingerprint(&first_prompt), "first_prompt");

        assert_eq!(base, fingerprint(&fake_catalog()));
    }

    #[test]
    fn token_returns_what_it_was_constructed_with() {
        let portal = Portal::new(fake_catalog(), "secret-token".into());
        assert_eq!(portal.token(), "secret-token");
    }
}
