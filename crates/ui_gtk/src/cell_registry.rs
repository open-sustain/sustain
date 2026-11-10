// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The one registry shape shared by every per-cell binding list in the UI.
//!
//! Several views need to find their realized cells again after creation:
//! the track table refreshes status icons, text values, and rating stars
//! in place, and inline editing hops between a row's editable cells on
//! Tab. Each keeps a registry of per-cell bindings, and they all share
//! the lifecycle implemented here.
//!
//! Bindings keep only weak widget references. GTK can tear down thousands
//! of cached cells when a large playlist shrinks to a small one; making
//! the registries self-pruning lets that teardown stay inside GTK instead
//! of firing one Sustain cleanup callback per cell. A walk upgrades each
//! binding's references once, drops the bindings that no longer upgrade
//! and visits the others with the upgraded widgets. This weak,
//! prune-on-walk design is part of the #226 playlist-switch fix:
//! reintroducing per-cell `teardown` removal (a registry scan on every
//! teardown) brings back the multi-second freeze, so keep the registries
//! weak and do not re-add an eager teardown path.
//!
//! Registries that are walked rarely (the editable-cell walk runs only on
//! Tab) would accumulate dead bindings without bound across playlist
//! switches, so [`push`] also sweeps dead bindings once the registry grows
//! past a watermark — amortized O(1) per registration, never tied to a
//! single cell's teardown.
//!
//! [`push`]: BindingRegistry::push

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

/// One realized cell tracked by a [`BindingRegistry`]. Each cell kind
/// parameterises the registry with its own payload — weak references to
/// the list item and widgets it refreshes, plus any per-cell state — and
/// upgrades those references for each visit.
pub(crate) trait CellBinding {
    /// The binding's list item and widgets, held strongly for one visit.
    type Live;

    /// The binding's list item and widgets, or `None` once any of them is
    /// gone: such a binding can never matter again and is dropped by the
    /// walk or watermark sweep that finds it so. A binding that is merely
    /// unbound (its cell is pooled for recycling) still upgrades: visitors
    /// tolerate the missing item, and the binding resumes service when GTK
    /// rebinds the cell.
    fn upgrade(&self) -> Option<Self::Live>;
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
/// once, at setup, and walks visit the live bindings while pruning the
/// dead ones. Cheaply cloneable; clones share one registry.
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

    /// Visits each live binding with its upgraded widgets, pruning the
    /// dead ones in the same pass. The borrow is held across the visit, so
    /// `visit` must not re-enter the registry.
    pub(crate) fn for_each_live(&self, mut visit: impl FnMut(&T, T::Live)) {
        let mut bindings = self.0.bindings.borrow_mut();
        bindings.retain(|binding| match binding.upgrade() {
            Some(live) => {
                visit(binding, live);
                true
            }
            None => false,
        });
        self.rearm_sweep(bindings.len());
    }

    /// The first `find` hit among the live bindings, pruning the dead ones
    /// in the same pass. The borrow is held across the search, so `find`
    /// must not re-enter the registry.
    pub(crate) fn find_map_live<R>(
        &self,
        mut find: impl FnMut(&T, T::Live) -> Option<R>,
    ) -> Option<R> {
        let mut found = None;
        self.for_each_live(|binding, live| {
            if found.is_none() {
                found = find(binding, live);
            }
        });
        found
    }

    fn sweep(&self, bindings: &mut Vec<T>) {
        bindings.retain(|binding| binding.upgrade().is_some());
        self.rearm_sweep(bindings.len());
    }

    fn rearm_sweep(&self, len: usize) {
        self.0.sweep_at.set((len * 2).max(MIN_SWEEP_LEN));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeBinding {
        id: usize,
        live: Rc<Cell<bool>>,
    }

    impl FakeBinding {
        fn new(id: usize, live: &Rc<Cell<bool>>) -> Self {
            Self {
                id,
                live: Rc::clone(live),
            }
        }
    }

    impl CellBinding for FakeBinding {
        type Live = ();

        fn upgrade(&self) -> Option<()> {
            self.live.get().then_some(())
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
        registry.for_each_live(|binding, ()| visited.push(binding.id));
        assert_eq!(visited, vec![1, 3]);
        assert_eq!(registry.0.bindings.borrow().len(), 2);
    }

    #[test]
    fn find_map_live_returns_the_first_hit() {
        let registry = BindingRegistry::default();
        let live = Rc::new(Cell::new(true));
        registry.push(FakeBinding::new(1, &live));
        registry.push(FakeBinding::new(2, &live));

        let hit = registry.find_map_live(|binding, ()| (binding.id == 2).then_some("hit"));
        assert_eq!(hit, Some("hit"));
        let miss = registry.find_map_live(|binding, ()| (binding.id == 9).then_some("hit"));
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
        for id in 1..=10 * MIN_SWEEP_LEN {
            registry.push(FakeBinding::new(id, &dead));
        }

        let len = registry.0.bindings.borrow().len();
        assert!(
            len <= 2 * MIN_SWEEP_LEN,
            "registry holds {len} bindings after the sweeps"
        );

        let mut visited = Vec::new();
        registry.for_each_live(|binding, ()| visited.push(binding.id));
        assert_eq!(visited, vec![0], "the live binding survives every sweep");
    }
}
