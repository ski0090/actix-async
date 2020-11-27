// Copy/paste from slab crate.
// The goal is to enable no_std.

use core::mem::replace;

use alloc::vec::Vec;

#[derive(Clone)]
pub struct Slab<T> {
    // Chunk of memory
    entries: Vec<Entry<T>>,

    // Number of Filled elements currently in the slab
    len: usize,

    // Offset of the next available slot in the slab. Set to the slab's
    // capacity when the slab is full.
    next: usize,
}

#[derive(Clone)]
enum Entry<T> {
    Vacant(usize),
    Occupied(T),
}

impl<T> Slab<T> {
    pub fn with_capacity(capacity: usize) -> Slab<T> {
        Slab {
            entries: Vec::with_capacity(capacity),
            next: 0,
            len: 0,
        }
    }

    pub fn get(&self, key: usize) -> Option<&T> {
        match self.entries.get(key) {
            Some(&Entry::Occupied(ref val)) => Some(val),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        match self.entries.get_mut(key) {
            Some(&mut Entry::Occupied(ref mut val)) => Some(val),
            _ => None,
        }
    }

    pub fn insert(&mut self, val: T) -> usize {
        let key = self.next;

        self.len += 1;

        if key == self.entries.len() {
            self.entries.push(Entry::Occupied(val));
            self.next = key + 1;
        } else {
            let prev = replace(&mut self.entries[key], Entry::Occupied(val));

            match prev {
                Entry::Vacant(next) => {
                    self.next = next;
                }
                _ => unreachable!(),
            }
        };

        key
    }

    pub fn remove(&mut self, key: usize) -> T {
        // Swap the entry at the provided value
        let prev = replace(&mut self.entries[key], Entry::Vacant(self.next));

        match prev {
            Entry::Occupied(val) => {
                self.len -= 1;
                self.next = key;
                val
            }
            _ => {
                // Woops, the entry is actually vacant, restore the state
                self.entries[key] = prev;
                panic!("invalid key");
            }
        }
    }

    pub fn contains(&self, key: usize) -> bool {
        match self.entries.get(key) {
            Some(Entry::Occupied(_)) => true,
            Some(_) | None => false,
        }
    }
}
