use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A small time-to-live cache. Values are cloned out, so cache `Arc`s for big things.
pub(crate) struct TtlCache<K, V> {
    ttl: Duration,
    items: Mutex<HashMap<K, (Instant, V)>>,
}

impl<K: Hash + Eq + Clone, V: Clone> TtlCache<K, V> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            items: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        let items = self.items.lock().ok()?;
        items
            .get(key)
            .filter(|(at, _)| at.elapsed() < self.ttl)
            .map(|(_, v)| v.clone())
    }

    pub fn put(&self, key: K, value: V) {
        if let Ok(mut items) = self.items.lock() {
            items.insert(key, (Instant::now(), value));
        }
    }

    pub fn remove(&self, key: &K) {
        if let Ok(mut items) = self.items.lock() {
            items.remove(key);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut items) = self.items.lock() {
            items.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_expire_and_can_be_removed() {
        let c = TtlCache::new(Duration::from_millis(40));
        c.put("a", 1);
        c.put("b", 2);
        assert_eq!(c.get(&"a"), Some(1));
        c.remove(&"a");
        assert_eq!(c.get(&"a"), None);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(c.get(&"b"), None, "expired");
        c.put("c", 3);
        c.clear();
        assert_eq!(c.get(&"c"), None);
    }
}
