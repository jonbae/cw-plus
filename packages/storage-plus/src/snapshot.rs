#![cfg(feature = "iterator")]

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use cosmwasm_std::{Order, StdError, StdResult, Storage};

use crate::keys::{PrimaryKey, U64Key};
use crate::map::Map;
use crate::path::Path;
use crate::prefix::Prefix;
use crate::Bound;

/// Map that maintains a snapshots of one or more checkpoints
pub struct SnapshotMap<'a, K, T> {
    primary: Map<'a, K, T>,

    // maps height to number of checkpoints (only used for selected)
    checkpoints: Map<'a, U64Key, u32>,

    // this stores all changes (key, height). Must differentiate between no data written,
    // and explicit None (just inserted)
    changelog: Map<'a, (K, U64Key), ChangeSet<T>>,

    // TODO: Selected not yet implemented
    strategy: Strategy,
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum Strategy {
    EveryBlock,
    Never,
    // Only writes for linked blocks - does a few more reads to save some writes.
    // Probably uses more gas, but less total disk usage
    Selected,
}

impl<'a, K, T> SnapshotMap<'a, K, T> {
    /// Usage: SnapshotMap::new(snapshot_names!("foobar"), Strategy::EveryBlock)
    pub const fn new(namespaces: SnapshotNamespaces<'a>, strategy: Strategy) -> Self {
        SnapshotMap {
            primary: Map::new(namespaces.pk),
            checkpoints: Map::new(namespaces.checkpoints),
            changelog: Map::new(namespaces.changelog),
            strategy,
        }
    }
}

impl<'a, K, T> SnapshotMap<'a, K, T>
where
    T: Serialize + DeserializeOwned + Clone,
    K: PrimaryKey<'a>,
{
    pub fn key(&self, k: K) -> Path<T> {
        self.primary.key(k)
    }

    pub fn prefix(&self, p: K::Prefix) -> Prefix<T> {
        self.primary.prefix(p)
    }

    /// is_checkpoint looks at the strategy and determines if we want to checkpoint
    fn is_checkpoint(&self, store: &dyn Storage, k: &K, height: u64) -> StdResult<bool> {
        match self.strategy {
            Strategy::EveryBlock => Ok(true),
            Strategy::Selected => unimplemented!(),
            Strategy::Never => Ok(false),
        }
    }

    /// load old value and store changelog
    fn write_change(&self, store: &mut dyn Storage, k: K, height: u64) -> StdResult<()> {
        let old = self.may_load(store, k.clone())?;
        self.changelog
            .save(store, (k, height.into()), &ChangeSet { old })
    }

    pub fn save(&self, store: &mut dyn Storage, k: K, data: &T, height: u64) -> StdResult<()> {
        if self.is_checkpoint(&store, &k, height) {
            self.write_change(store, k.clone(), height)?;
        }
        self.primary.save(store, k, data)
    }

    pub fn remove(&self, store: &mut dyn Storage, k: K, height: u64) -> StdResult<()> {
        if self.is_checkpoint(&store, &k, height) {
            self.write_change(store, k.clone(), height)?;
        }
        self.primary.remove(store, k);
        Ok(())
    }

    /// load will return an error if no data is set at the given key, or on parse error
    pub fn load(&self, store: &dyn Storage, k: K) -> StdResult<T> {
        self.primary.load(store, k)
    }

    /// may_load will parse the data stored at the key if present, returns Ok(None) if no data there.
    /// returns an error on issues parsing
    pub fn may_load(&self, store: &dyn Storage, k: K) -> StdResult<Option<T>> {
        self.primary.may_load(store, k)
    }

    // may_load_at_height reads historical data from given checkpoints.
    // only guaranteed to give correct data if Strategy::EveryBlock or
    // Strategy::Selected and h element of checkpoint heights
    pub fn may_load_at_height(
        &self,
        store: &dyn Storage,
        k: K,
        height: u64,
    ) -> StdResult<Option<T>> {
        // this will look for the first snapshot of the given address >= given height
        // If None, there is no snapshot since that time.
        let start = Bound::inclusive(U64Key::new(height));
        let first = self
            .changelog
            .prefix(k.clone())
            .range(storage, Some(start), None, Order::Ascending)
            .next();

        if let Some(r) = first {
            // if we found a match, return this last one
            r.map(|(_, v)| v.old)
        } else {
            // otherwise, return current value
            self.may_load(store, k)
        }
    }

    /// Loads the data, perform the specified action, and store the result
    /// in the database. This is shorthand for some common sequences, which may be useful.
    ///
    /// If the data exists, `action(Some(value))` is called. Otherwise `action(None)` is called.
    ///
    /// This is a bit more customized than needed to only read "old" value 1 time, not 2 per naive approach
    pub fn update<A, E>(
        &self,
        store: &mut dyn Storage,
        k: K,
        height: u64,
        action: A,
    ) -> Result<T, E>
    where
        A: FnOnce(Option<T>) -> Result<T, E>,
        E: From<StdError>,
    {
        let input = self.may_load(store, k.clone())?;
        let diff = ChangeSet { old: input.clone() };

        let output = action(input)?;
        // optimize the save (save the extra read in write_change)
        if self.is_checkpoint(&store, &k, height) {
            self.changelog
                .save(store, (k.clone(), height.into()), &diff)?;
        }
        self.primary.save(store, k, &output);

        Ok(output)
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
struct ChangeSet<T> {
    pub old: Option<T>,
}

pub struct SnapshotNamespaces<'a> {
    pub pk: &'a [u8],
    pub checkpoints: &'a [u8],
    pub changelog: &'a [u8],
}

#[macro_export]
macro_rules! snapshot_names {
    ($var:expr) => {
        SnapshotNamespaces {
            pk: $var.as_bytes(),
            checkpoints: concat!($var, "__checkpoints").as_bytes(),
            changelog: concat!($var, "__changelog").as_bytes(),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmwasm_std::testing::MockStorage;

    #[test]
    fn namespace_macro() {
        let names = snapshot_names!("demo");
        assert_eq!(names.pk, b"demo");
        assert_eq!(names.checkpoints, b"demo__checkpoints");
        assert_eq!(names.changelog, b"demo__changelog");
    }

    type TestMap = SnapshotMap<'static, &'static [u8], u64>;
    const NEVER: TestMap = SnapshotMap::new(snapshot_names!("never"), Strategy::Never);
    const EVERY: TestMap = SnapshotMap::new(snapshot_names!("every"), Strategy::EveryBlock);
    const SELECT: TestMap = SnapshotMap::new(snapshot_names!("select"), Strategy::Selected);

    // Fills a map &[u8] -> u64 with the following writes:
    // 1: A = 5
    // 2: B = 7
    // 3: C = 1, A = 8
    // 4: B = None, C = 13
    // 5: A = None, D = 22
    // Final values -> C = 13, D = 22
    fn init_data(map: &TestMap, storage: &mut dyn Storage) {
        map.save(storage, b"A", &5, 1).unwrap();
        map.save(storage, b"B", &7, 2).unwrap();

        // also use update to set - to ensure this works
        map.save(storage, b"C", &1, 3).unwrap();
        map.update(storage, b"A", 3, |_| Ok(8)).unwrap();

        map.remove(storage, b"B", 4).unwrap();
        map.save(storage, b"C", &13, 4).unwrap();

        map.remove(storage, b"A", 5).unwrap();
        map.update(storage, b"D", 5, |_| Ok(22)).unwrap();
    }

    fn assert_final_values(map: &TestMap, storage: &mut dyn Storage) {
        assert_eq!(None, map.may_load(storage, b"A"));
        assert_eq!(None, map.may_load(storage, b"B"));
        assert_eq!(Some(13u64), map.may_load(storage, b"C"));
        assert_eq!(Some(22u64), map.may_load(storage, b"D"));
    }

    #[test]
    fn never_works_like_normal_map() {
        let mut storage = MockStorage::new();
        init_data(&NEVER, &mut storage);
        assert_final_values(&NEVER, &mut storage);
    }

    #[test]
    fn every_blocks_stores_present_and_past() {
        let mut storage = MockStorage::new();
        init_data(&NEVER, &mut storage);
        assert_final_values(&NEVER, &mut storage);
    }
}
