// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The one registry shape shared by every per-cell binding list in the UI.
//!
//! Several views need to find their realized cells again after creation:
//! the track table refreshes status icons, text values, and rating stars
//! in place, context menus hit-test the cell under a right-click, and
//! inline editing hops between a row's editable cells on Tab. Each keeps a
//! registry of per-cell bindings, and they all share the lifecycle
//! implemented here.
//!
//! Bindings keep only weak widget references. GTK can tear down thousands
//! of cached cells when a large playlist shrinks to a small one; making
//! the registries self-pruning lets that teardown stay inside GTK instead
//! of firing one Sustain cleanup callback per cell. This weak,
//! prune-on-walk design is part of the #226 playlist-switch fix:
//! reintroducing per-cell `teardown` removal (a registry scan on every
//! teardown) brings back the multi-second freeze, so keep the registries
//! weak and do not re-add an eager teardown path.
//!
//! Registries that are walked rarely (a context menu's hit-test runs only
//! on right-click, the editable-cell walk only on Tab) would accumulate
//! dead bindings without bound across playlist switches, so [`push`] also
//! sweeps dead bindings once the registry grows past a watermark —
//! amortized O(1) per registration, never tied to a single cell's
//! teardown.
//!
//! [`push`]: BindingRegistry::push

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::ObjectType;

/// One realized cell tracked by a [`BindingRegistry`]. Each cell kind
/// parameterises the registry with its own payload — the widgets it
/// refreshes plus any per-cell state — and answers the registry's two
/// questions: which list-item slot the cell occupies, and whether the
/// binding is still worth keeping.
pub(crate) trait CellBinding {
    /// Stable identity of the binding's list-item slot (its pointer
    /// value), used by [`BindingRegistry::replace`] to drop a superseded
    /// registration for the same slot.
    fn key(&self) -> usize;

    /// The list item the cell occupies, if it is still alive.
    fn list_item(&self) -> Option<gtk::ListItem>;

    /// Whether the binding's widgets still exist. A binding whose widgets
    /// were destroyed can never matter again and is dropped by the next
    /// walk or watermark sweep. A binding that is merely unbound (its cell
    /// is pooled for recycling) must report `true`: visitors tolerate the
    /// missing item, and the binding resumes service when GTK rebinds the
    /// cell.
    fn is_live(&self) -> bool;
}

/// Floor for the sweep watermark, so small registries never bother.
const MIN_SWEEP_LEN: usize = 32;

struct RegistryInner<T> {
    bindings: RefCell<Vec<T>>,
    /// Registry length at which the next [`BindingRegistry::push`] sweeps
    /// dead bindings. Reset to twice the surviving length after every
    /// sweep, which keeps sweeping amortized O(1) per registration.
    sweep_at: Cell<usize>,
}

/// The lifecycle every cell registry shares: a cell pushes its binding in
/// at setup (or re-registers on bind via [`replace`]), and walks prune
/// dead bindings before visiting the survivors. Cheaply cloneable; clones
/// share one registry.
///
/// [`replace`]: BindingRegistry::replace
pub(crate) struct BindingRegistry<T>(Rc<RegistryInner<T>>);

impl<T> Clone for BindingRegistry<T> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

impl<T> Default for BindingRegistry<T> {
    fn default() -> Self {
        Self(Rc::new(RegistryInner {
            bindings: RefCell::new(Vec::new()),
            sweep_at: Cell::new(MIN_SWEEP_LEN),
        }))
    }
}

impl<T: CellBinding> BindingRegistry<T> {
    pub(crate) fn push(&self, binding: T) {
        let mut bindings = self.0.bindings.borrow_mut();
        bindings.push(binding);
        if bindings.len() >= self.0.sweep_at.get() {
            self.sweep(&mut bindings);
        }
    }

    /// Replaces any binding already registered for this list-item slot.
    /// Rating cells re-register on every bind as the cell is recycled, so
    /// a stale entry for the same slot must not pile up.
    pub(crate) fn replace(&self, binding: T) {
        let key = binding.key();
        let mut bindings = self.0.bindings.borrow_mut();
        bindings.retain(|existing| existing.key() != key);
        bindings.push(binding);
        if bindings.len() >= self.0.sweep_at.get() {
            self.sweep(&mut bindings);
        }
    }

    /// Prunes dead bindings, then visits each survivor. The borrow is held
    /// across the visit, so `visit` must not re-enter the registry.
    pub(crate) fn for_each_live(&self, mut visit: impl FnMut(&T)) {
        let mut bindings = self.0.bindings.borrow_mut();
        self.sweep(&mut bindings);
        for binding in bindings.iter() {
            visit(binding);
        }
    }

    /// Prunes dead bindings, then returns the first `find` hit among the
    /// survivors. The borrow is held across the search, so `find` must not
    /// re-enter the registry.
    pub(crate) fn find_map_live<R>(&self, find: impl FnMut(&T) -> Option<R>) -> Option<R> {
        let mut bindings = self.0.bindings.borrow_mut();
        self.sweep(&mut bindings);
        bindings.iter().find_map(find)
    }

    fn sweep(&self, bindings: &mut Vec<T>) {
        bindings.retain(T::is_live);
        self.0.sweep_at.set((bindings.len() * 2).max(MIN_SWEEP_LEN));
    }
}

/// The registry key for a cell occupying `list_item`: the list item's
/// pointer value. GTK may reuse a freed list item's address for a new one,
/// which is harmless here — the key is only used to *replace* a stale
/// registration for the same slot.
pub(crate) fn list_item_key(list_item: &gtk::ListItem) -> usize {
    list_item.as_ptr() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeBinding {
        key: usize,
        live: Rc<Cell<bool>>,
    }

    impl FakeBinding {
        fn new(key: usize, live: &Rc<Cell<bool>>) -> Self {
            Self {
                key,
                live: Rc::clone(live),
            }
        }
    }

    impl CellBinding for FakeBinding {
        fn key(&self) -> usize {
            self.key
        }

        fn list_item(&self) -> Option<gtk::ListItem> {
            None
        }

        fn is_live(&self) -> bool {
            self.live.get()
        }
    }

    #[test]
    fn walks_prune_dead_bindings_and_visit_survivors() {
        let registry = BindingRegistry::default();
        let live = Rc::new(Cell::new(true));
        let dead = Rc::new(Cell::new(false));
        registry.push(FakeBinding::new(1, &live));
        registry.push(FakeBinding::new(2, &dead));
        registry.push(FakeBinding::new(3, &live));

        let mut visited = Vec::new();
        registry.for_each_live(|binding| visited.push(binding.key()));
        assert_eq!(visited, vec![1, 3]);
        assert_eq!(registry.0.bindings.borrow().len(), 2);
    }

    #[test]
    fn replace_drops_the_previous_binding_for_the_same_slot() {
        let registry = BindingRegistry::default();
        let live = Rc::new(Cell::new(true));
        registry.push(FakeBinding::new(1, &live));
        registry.replace(FakeBinding::new(1, &live));
        registry.replace(FakeBinding::new(2, &live));

        let mut visited = Vec::new();
        registry.for_each_live(|binding| visited.push(binding.key()));
        assert_eq!(visited, vec![1, 2]);
    }

    #[test]
    fn find_map_live_returns_the_first_hit() {
        let registry = BindingRegistry::default();
        let live = Rc::new(Cell::new(true));
        registry.push(FakeBinding::new(1, &live));
        registry.push(FakeBinding::new(2, &live));

        let hit = registry.find_map_live(|binding| (binding.key() == 2).then_some("hit"));
        assert_eq!(hit, Some("hit"));
        let miss = registry.find_map_live(|binding| (binding.key() == 9).then_some("hit"));
        assert_eq!(miss, None);
    }

    #[test]
    fn pushes_sweep_dead_bindings_at_the_watermark() {
        let registry = BindingRegistry::default();
        let live = Rc::new(Cell::new(true));
        let dead = Rc::new(Cell::new(false));

        // A registry that is never walked (no Tab, no right-click) must not
        // grow without bound as cells are torn down and recreated across
        // playlist switches.
        registry.push(FakeBinding::new(0, &live));
        for key in 1..=10 * MIN_SWEEP_LEN {
            registry.push(FakeBinding::new(key, &dead));
        }

        let len = registry.0.bindings.borrow().len();
        assert!(
            len <= 2 * MIN_SWEEP_LEN,
            "registry holds {len} bindings after the sweeps"
        );

        let mut visited = Vec::new();
        registry.for_each_live(|binding| visited.push(binding.key()));
        assert_eq!(visited, vec![0], "the live binding survives every sweep");
    }
}
