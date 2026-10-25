//! Typed VA object IDs with constant-time lookup. IDs are monotonically
//! allocated and never reused within a display, so a destroyed object's
//! 32-bit ID cannot alias a replacement (truncating slotmap keys lost their
//! generation bits). Exhaustion returns an allocation error instead of wrapping.
#![allow(dead_code)]

use crate::error::DriverError;
use slotmap::{Key, KeyData, new_key_type};
use std::collections::HashMap;
use std::ops::{Index, IndexMut};

new_key_type! {
    /// Slotmap key for entries in [`Pools::configs`].
    pub struct ConfigKey;
    /// Slotmap key for entries in [`Pools::surfaces`].
    pub struct SurfaceKey;
    /// Slotmap key for entries in [`Pools::contexts`].
    pub struct ContextKey;
    /// Slotmap key for entries in [`Pools::buffers`].
    pub struct BufferKey;
    /// Slotmap key for entries in [`Pools::images`].
    pub struct ImageKey;
}

pub struct IdPool<K: Key, V> {
    entries: HashMap<K, V>,
    next_id: u32,
}

impl<K: Key, V> IdPool<K, V> {
    pub fn with_key() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 1,
        }
    }
    pub fn insert(&mut self, value: V) -> Result<K, DriverError> {
        if self.next_id == u32::MAX {
            return Err(DriverError::AllocFailed);
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| DriverError::AllocFailed)?;
        let key = key_for_id(self.next_id);
        self.next_id += 1;
        self.entries.insert(key, value);
        Ok(key)
    }
    pub fn get(&self, key: K) -> Option<&V> {
        self.entries.get(&key)
    }
    pub fn remove(&mut self, key: K) -> Option<V> {
        self.entries.remove(&key)
    }
    pub fn contains_key(&self, key: K) -> bool {
        self.entries.contains_key(&key)
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        self.entries.iter().map(|(key, value)| (*key, value))
    }
}
impl<K: Key, V> Index<K> for IdPool<K, V> {
    type Output = V;
    fn index(&self, key: K) -> &V {
        &self.entries[&key]
    }
}
impl<K: Key, V> IndexMut<K> for IdPool<K, V> {
    fn index_mut(&mut self, key: K) -> &mut V {
        self.entries.get_mut(&key).expect("live object key")
    }
}
fn key_for_id<K: Key>(id: u32) -> K {
    KeyData::from_ffi(u64::from(id) | (1 << 32)).into()
}

#[inline]
pub fn key_to_id<K: Key>(key: K) -> u32 {
    key.data().as_ffi() as u32
}

pub fn id_to_key<K: Key>(id: u32, pool: &impl ContainsKey<K>) -> Option<K> {
    pool.find_by_low_bits(id)
}
pub trait ContainsKey<K: Key> {
    fn find_by_low_bits(&self, id: u32) -> Option<K>;
}
impl<K: Key, V> ContainsKey<K> for IdPool<K, V> {
    fn find_by_low_bits(&self, id: u32) -> Option<K> {
        let key = key_for_id(id);
        self.entries.contains_key(&key).then_some(key)
    }
}

/// Reconstruct a key from a lossless 64-bit round-trip of `KeyData::as_ffi`.
///
/// Used when the full 64-bit `KeyData` value has been stored somewhere
/// (e.g. a `CodedBuffer` back-reference kept inside another record) and needs
/// to be recovered exactly. This is distinct from [`id_to_key`], which handles
/// the lossy 32-bit VA ID space.
#[inline]
pub fn key_from_raw<K: Key>(raw: u64) -> K {
    KeyData::from_ffi(raw).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_roundtrip_without_reusing_destroyed_objects() {
        let mut pool = IdPool::<ConfigKey, _>::with_key();
        let old = pool.insert(42).unwrap();
        let old_id = key_to_id(old);
        assert_eq!(pool.find_by_low_bits(old_id), Some(old));
        assert_eq!(pool.remove(old), Some(42));
        for _ in 0..1000 {
            let key = pool.insert(7).unwrap();
            assert_ne!(key_to_id(key), old_id);
            assert!(pool.find_by_low_bits(old_id).is_none());
            assert_eq!(pool.find_by_low_bits(key_to_id(key)), Some(key));
            pool.remove(key);
        }
    }
    #[test]
    fn clear_keeps_the_id_sequence_and_exhaustion_never_wraps() {
        let mut pool = IdPool::<BufferKey, _>::with_key();
        let old = pool.insert(()).unwrap();
        pool.clear();
        let new = pool.insert(()).unwrap();
        assert_ne!(key_to_id(new), key_to_id(old));
        pool.next_id = u32::MAX - 1;
        assert_eq!(key_to_id(pool.insert(()).unwrap()), u32::MAX - 1);
        assert!(pool.insert(()).is_err());
        assert!(pool.find_by_low_bits(0).is_none());
        assert!(pool.find_by_low_bits(u32::MAX).is_none());
    }
    #[test]
    fn key_from_raw_roundtrips_via_full_64bit_ffi() {
        let mut pool = IdPool::<SurfaceKey, _>::with_key();
        let key = pool.insert(()).unwrap();
        let rebuilt: SurfaceKey = key_from_raw(key.data().as_ffi());
        assert_eq!(key, rebuilt);
        assert!(pool.contains_key(rebuilt));
    }
}
