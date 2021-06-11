//! A vector implemented on a trie. Unlike standard vector does not support insertion and removal
//! of an element results in the last element being placed in the empty position.
// TODO update these docs

mod impls;
mod iter;

use borsh::{BorshDeserialize, BorshSerialize};
use once_cell::unsync::OnceCell;
use std::cell::RefCell;
use std::collections::BTreeMap;

use self::iter::{Iter, IterMut};
use crate::collections::append_slice;
use crate::{env, CacheEntry, EntryState, IntoStorageKey};

const ERR_INCONSISTENT_STATE: &[u8] = b"The collection is an inconsistent state. Did previous smart contract execution terminate unexpectedly?";
const ERR_ELEMENT_DESERIALIZATION: &[u8] = b"Cannot deserialize element";
const ERR_ELEMENT_SERIALIZATION: &[u8] = b"Cannot serialize element";
const ERR_INDEX_OUT_OF_BOUNDS: &[u8] = b"Index out of bounds";

fn expect_consistent_state<T>(val: Option<T>) -> T {
    val.unwrap_or_else(|| env::panic(ERR_INCONSISTENT_STATE))
}

struct StableMap<K, V> {
    map: RefCell<BTreeMap<K, Box<V>>>,
}

impl<K: Ord, V> Default for StableMap<K, V> {
    fn default() -> Self {
        StableMap { map: Default::default() }
    }
}

impl<K, V> StableMap<K, V> {
    fn get(&self, k: K) -> &V
    where
        K: Ord,
        V: Default,
    {
        let mut map = self.map.borrow_mut();
        let v: &mut Box<V> = map.entry(k).or_default();
        let v: &V = &*v;
        // SAFETY: here, we extend the lifetime of `V` from local `RefCell`
        // borrow to the `&self`. This is valid because we only append to the
        // map via `&` reference, and the values are boxed, so we have stability
        // of addresses.
        unsafe { &*(v as *const V) }
    }
    fn get_mut(&mut self, k: K) -> &mut V
    where
        K: Ord,
        V: Default,
    {
        &mut *self.map.get_mut().entry(k).or_default()
    }
    fn inner(&mut self) -> &mut BTreeMap<K, Box<V>> {
        self.map.get_mut()
    }
}

/// An iterable implementation of vector that stores its content on the trie.
/// Uses the following map: index -> element.
///
/// This implementation will cache all changes and loads and only updates values that are changed
/// in storage after it's dropped through it's [`Drop`] implementation.
///
/// TODO examples
#[derive(BorshSerialize, BorshDeserialize)]
pub struct Vector<T>
where
    T: BorshSerialize,
{
    len: u32,
    prefix: Vec<u8>,
    #[borsh_skip]
    /// Cache for loads and intermediate changes to the underlying vector.
    /// The cached entries are wrapped in a [`Box`] to avoid existing pointers from being
    /// invalidated.
    cache: StableMap<u32, OnceCell<CacheEntry<T>>>,
}

impl<T> Vector<T>
where
    T: BorshSerialize,
{
    /// Returns the number of elements in the vector, also referred to as its size.
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Returns `true` if the vector contains no elements.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Create new vector with zero elements. Use `id` as a unique identifier on the trie.
    pub fn new<S>(prefix: S) -> Self
    where
        S: IntoStorageKey,
    {
        Self { len: 0, prefix: prefix.into_storage_key(), cache: Default::default() }
    }

    fn index_to_lookup_key(&self, index: u32) -> Vec<u8> {
        append_slice(&self.prefix, &index.to_le_bytes()[..])
    }

    /// Removes all elements from the collection. This will remove all storage values for the
    /// length of the [`Vector`].
    pub fn clear(&mut self) {
        for i in 0..self.len {
            let lookup_key = self.index_to_lookup_key(i);
            env::storage_remove(&lookup_key);
        }
        self.len = 0;
        self.cache.inner().clear();
    }

    // TODO expose this? Could be useful to not force a user to drop to persist changes
    /// Flushes the cache and writes all modified values to storage.
    fn flush(&mut self) {
        for (k, v) in self.cache.inner().iter_mut() {
            if let Some(v) = v.get_mut() {
                if v.is_modified() {
                    let key = append_slice(&self.prefix, &k.to_le_bytes()[..]);
                    match v.value().as_ref() {
                        Some(modified) => {
                            // Value was modified, write the updated value to storage
                            env::storage_write(&key, &Self::serialize_element(modified));
                        }
                        None => {
                            // Element was removed, clear the storage for the value
                            env::storage_remove(&key);
                        }
                    }

                    // Update state of flushed state as cached, to avoid duplicate writes/removes
                    // while also keeping the cached values in memory.
                    v.replace_state(EntryState::Cached);
                }
            }
        }
    }

    /// Sets a value at a given index to the value provided. This does not shift values after the
    /// index to the right.
    pub fn set(&mut self, index: u32, value: T) {
        if index >= self.len() {
            env::panic(ERR_INDEX_OUT_OF_BOUNDS);
        }

        let entry = self.cache.get_mut(index);
        match entry.get_mut() {
            Some(entry) => *entry.value_mut() = Some(value),
            None => {
                let _ = entry.set(CacheEntry::new_modified(Some(value)));
            }
        }
    }

    fn serialize_element(element: &T) -> Vec<u8> {
        element.try_to_vec().unwrap_or_else(|_| env::panic(ERR_ELEMENT_SERIALIZATION))
    }

    /// Appends an element to the back of the collection.
    pub fn push(&mut self, element: T) {
        if self.len() >= u32::MAX {
            env::panic(ERR_INDEX_OUT_OF_BOUNDS);
        }

        let last_idx = self.len();
        self.len += 1;
        self.set(last_idx, element)
    }
}

impl<T> Vector<T>
where
    T: BorshSerialize + BorshDeserialize,
{
    fn deserialize_element(raw_element: &[u8]) -> T {
        T::try_from_slice(&raw_element).unwrap_or_else(|_| env::panic(ERR_ELEMENT_DESERIALIZATION))
    }

    /// Returns the element by index or `None` if it is not present.
    pub fn get(&self, index: u32) -> Option<&T> {
        // TODO doc safety
        let entry = self.cache.get(index).get_or_init(|| {
            let storage_bytes = env::storage_read(&self.index_to_lookup_key(index));
            let value = storage_bytes.as_deref().map(Self::deserialize_element);
            CacheEntry::new_cached(value)
        });
        entry.value().as_ref()
    }

    /// Returns a mutable reference to the element at the `index` provided.
    fn get_mut_inner(&mut self, index: u32) -> &mut CacheEntry<T> {
        let index_to_lookup_key = self.index_to_lookup_key(index);
        let entry = self.cache.get_mut(index);
        entry.get_or_init(|| {
            let storage_bytes = env::storage_read(&index_to_lookup_key);
            let value = storage_bytes.as_deref().map(Self::deserialize_element);
            CacheEntry::new_cached(value)
        });
        let entry = entry.get_mut().unwrap();
        entry
    }

    /// Returns a mutable reference to the element at the `index` provided.
    pub fn get_mut(&mut self, index: u32) -> Option<&mut T> {
        let entry = self.get_mut_inner(index);
        entry.value_mut().as_mut()
    }

    fn swap(&mut self, a: u32, b: u32) {
        if a >= self.len() || b >= self.len() {
            env::panic(ERR_INDEX_OUT_OF_BOUNDS);
        }

        if a == b {
            // Short circuit if indices are the same, also guarantees uniqueness below
            return;
        }

        let val_a = self.get_mut_inner(a).replace(None);
        let val_b = self.get_mut_inner(b).replace(val_a);
        self.get_mut_inner(a).replace(val_b);
    }

    /// Removes an element from the vector and returns it.
    /// The removed element is replaced by the last element of the vector.
    /// Does not preserve ordering, but is `O(1)`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of bounds.
    pub fn swap_remove(&mut self, index: u32) -> T {
        if self.is_empty() {
            env::panic(ERR_INDEX_OUT_OF_BOUNDS);
        }

        self.swap(index, self.len() - 1);
        expect_consistent_state(self.pop())
    }

    /// Removes the last element from a vector and returns it, or `None` if it is empty.
    pub fn pop(&mut self) -> Option<T> {
        if self.is_empty() {
            None
        } else {
            let last_idx = self.len - 1;
            self.len = last_idx;

            // Replace current value with none, and return the existing value
            self.get_mut_inner(last_idx).replace(None)
        }
    }

    // TODO this is not an API that matches std or anything, but is kept because it exists in
    // the previous version. You can achieve the same with `get_mut` and `mem::replace`
    /// Inserts a element at `index`, returns an evicted element.
    ///
    /// # Panics
    ///
    /// If `index` is out of bounds.
    pub fn replace(&mut self, index: u32, element: T) -> T {
        self.get_mut_inner(index).replace(Some(element)).unwrap()
    }

    /// Returns an iterator over the vector. This iterator will lazily load any values iterated
    /// over from storage.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter::new(self)
    }

    /// Returns an iterator over the [`Vector`] that allows modifying each value. This iterator
    /// will lazily load any values iterated over from storage.
    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        IterMut::new(self)
    }
}

#[cfg(not(feature = "expensive-debug"))]
impl<T> std::fmt::Debug for Vector<T>
where
    T: BorshSerialize + std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vector").field("len", &self.len).field("prefix", &self.prefix).finish()
    }
}

#[cfg(feature = "expensive-debug")]
impl<T: std::fmt::Debug + BorshDeserialize> std::fmt::Debug for Vector<T>
where
    T: BorshSerialize,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.iter().collect::<Vec<_>>().fmt(f)
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(test)]
mod tests {
    use rand::{Rng, SeedableRng};

    use super::Vector;
    use crate::test_utils::test_env;

    #[test]
    fn test_push_pop() {
        test_env::setup();
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(0);
        let mut vec = Vector::new(b"v".to_vec());
        let mut baseline = vec![];
        for _ in 0..500 {
            let value = rng.gen::<u64>();
            vec.push(value);
            baseline.push(value);
        }
        let actual: Vec<u64> = vec.iter().cloned().collect();
        assert_eq!(actual, baseline);
        for _ in 0..1001 {
            assert_eq!(baseline.pop(), vec.pop());
        }
    }

    #[test]
    pub fn test_replace() {
        test_env::setup();
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(1);
        let mut vec = Vector::new(b"v".to_vec());
        let mut baseline = vec![];
        for _ in 0..500 {
            let value = rng.gen::<u64>();
            vec.push(value);
            baseline.push(value);
        }
        for _ in 0..500 {
            let index = rng.gen::<u32>() % vec.len();
            let value = rng.gen::<u64>();
            let old_value0 = vec[index];
            let old_value1 = core::mem::replace(vec.get_mut(index).unwrap(), value);
            let old_value2 = baseline[index as usize];
            assert_eq!(old_value0, old_value1);
            assert_eq!(old_value0, old_value2);
            *baseline.get_mut(index as usize).unwrap() = value;
        }
        let actual: Vec<_> = vec.iter().cloned().collect();
        assert_eq!(actual, baseline);
    }

    #[test]
    pub fn test_swap_remove() {
        test_env::setup();
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(2);
        let mut vec = Vector::new(b"v".to_vec());
        let mut baseline = vec![];
        for _ in 0..500 {
            let value = rng.gen::<u64>();
            vec.push(value);
            baseline.push(value);
        }
        for _ in 0..500 {
            let index = rng.gen::<u32>() % vec.len();
            let old_value0 = vec[index];
            let old_value1 = vec.swap_remove(index);
            let old_value2 = baseline[index as usize];
            let last_index = baseline.len() - 1;
            baseline.swap(index as usize, last_index);
            baseline.pop();
            assert_eq!(old_value0, old_value1);
            assert_eq!(old_value0, old_value2);
        }
        let actual: Vec<_> = vec.iter().cloned().collect();
        assert_eq!(actual, baseline);
    }

    #[test]
    pub fn test_clear() {
        test_env::setup();
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(3);
        let mut vec = Vector::new(b"v".to_vec());
        for _ in 0..100 {
            for _ in 0..(rng.gen::<u64>() % 20 + 1) {
                let value = rng.gen::<u64>();
                vec.push(value);
            }
            assert!(!vec.is_empty());
            vec.clear();
            assert!(vec.is_empty());
        }
    }

    #[test]
    pub fn test_extend() {
        test_env::setup();
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(0);
        let mut vec = Vector::new(b"v".to_vec());
        let mut baseline = vec![];
        for _ in 0..100 {
            let value = rng.gen::<u64>();
            vec.push(value);
            baseline.push(value);
        }

        for _ in 0..100 {
            let mut tmp = vec![];
            for _ in 0..=(rng.gen::<u64>() % 20 + 1) {
                let value = rng.gen::<u64>();
                tmp.push(value);
            }
            baseline.extend(tmp.clone());
            vec.extend(tmp.clone());
        }
        let actual: Vec<_> = vec.iter().cloned().collect();
        assert_eq!(actual, baseline);
    }

    #[test]
    fn test_debug() {
        test_env::setup();
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(4);
        let prefix = b"v".to_vec();
        let mut vec = Vector::new(prefix.clone());
        let mut baseline = vec![];
        for _ in 0..10 {
            let value = rng.gen::<u64>();
            vec.push(value);
            baseline.push(value);
        }
        let actual: Vec<_> = vec.iter().cloned().collect();
        assert_eq!(actual, baseline);
        for _ in 0..5 {
            assert_eq!(baseline.pop(), vec.pop());
        }
        if cfg!(feature = "expensive-debug") {
            assert_eq!(format!("{:#?}", vec), format!("{:#?}", baseline));
        } else {
            assert_eq!(
                format!("{:?}", vec),
                format!("Vector {{ len: 5, prefix: {:?} }}", vec.prefix)
            );
        }

        // * The storage is reused in the second part of this test, need to flush
        vec.flush();

        use borsh::{BorshDeserialize, BorshSerialize};
        #[derive(Debug, BorshSerialize, BorshDeserialize)]
        struct TestType(u64);

        let deserialize_only_vec =
            Vector::<TestType> { len: vec.len(), prefix, cache: Default::default() };
        let baseline: Vec<_> = baseline.into_iter().map(|x| TestType(x)).collect();
        if cfg!(feature = "expensive-debug") {
            assert_eq!(format!("{:#?}", deserialize_only_vec), format!("{:#?}", baseline));
        } else {
            assert_eq!(
                format!("{:?}", deserialize_only_vec),
                format!("Vector {{ len: 5, prefix: {:?} }}", deserialize_only_vec.prefix)
            );
        }
    }
}
