//! A value worked out once and kept until what it was worked out from
//! changes, for tables the UI would otherwise rebuild every frame.

use std::cell::RefCell;
use std::rc::Rc;

/// The last value `get` worked out, and the key it was worked out for.
pub struct Memo<K, V> {
    kept: RefCell<Option<(K, Rc<V>)>>,
}

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Memo {
            kept: RefCell::new(None),
        }
    }
}

impl<K: PartialEq, V> Memo<K, V> {
    /// The value for `key`: the kept one when it was worked out for an
    /// equal key, else what `work_out` gives, kept from now on.
    pub fn get(&self, key: K, work_out: impl FnOnce() -> V) -> Rc<V> {
        let mut kept = self.kept.borrow_mut();
        if let Some((kept_key, value)) = kept.as_ref()
            && *kept_key == key
        {
            return value.clone();
        }
        let value = Rc::new(work_out());
        *kept = Some((key, value.clone()));
        value
    }
}

#[cfg(test)]
mod tests {
    // A memo asked for values under keys, counting how often it works one
    // out.
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_value_is_worked_out_again_only_for_another_key() {
        // Two asks under one key work the value out once; a new key works
        // it out again, and the old key after it once more, since only
        // the last is kept.
        let memo: Memo<u64, String> = Memo::default();
        let worked = Cell::new(0);
        let ask = |key: u64| {
            memo.get(key, || {
                worked.set(worked.get() + 1);
                format!("rows for {key}")
            })
        };
        assert_eq!(*ask(1), "rows for 1");
        assert_eq!(*ask(1), "rows for 1");
        assert_eq!(worked.get(), 1);
        assert_eq!(*ask(2), "rows for 2");
        assert_eq!(*ask(1), "rows for 1");
        assert_eq!(worked.get(), 3);
    }
}
