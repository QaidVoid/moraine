//! A small thread-safe string interner.
//!
//! Category, package, and USE-flag tokens repeat across tens of thousands of
//! packages. Interning them once gives every later crate a cheap [`Copy`]
//! handle to compare and hash instead of repeated heap strings.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A handle to an interned string, cheap to copy, compare, and hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Symbol(u32);

impl Symbol {
    /// The numeric index of this symbol within its interner.
    pub fn index(self) -> u32 {
        self.0
    }
}

/// Interns strings, returning a stable [`Symbol`] for each distinct string.
#[derive(Debug, Default)]
pub struct Interner {
    inner: RwLock<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    lookup: HashMap<Arc<str>, Symbol>,
    strings: Vec<Arc<str>>,
}

impl Interner {
    /// Create an empty interner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `s`, returning a stable [`Symbol`] for it. Repeated calls with an
    /// equal string return the same symbol.
    pub fn intern(&self, s: &str) -> Symbol {
        if let Some(&sym) = self.read().lookup.get(s) {
            return sym;
        }
        let mut inner = self.write();
        // Re-check under the write lock in case of a concurrent insert.
        if let Some(&sym) = inner.lookup.get(s) {
            return sym;
        }
        let sym = Symbol(inner.strings.len() as u32);
        let shared: Arc<str> = Arc::from(s);
        inner.strings.push(Arc::clone(&shared));
        inner.lookup.insert(shared, sym);
        sym
    }

    /// Resolve a [`Symbol`] back to its string, if it belongs to this interner.
    pub fn resolve(&self, sym: Symbol) -> Option<Arc<str>> {
        self.read().strings.get(sym.0 as usize).cloned()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_string_interns_to_same_symbol() {
        let interner = Interner::new();
        let a = interner.intern("dev-lang/rust");
        let b = interner.intern("dev-lang/rust");
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_strings_get_distinct_symbols() {
        let interner = Interner::new();
        let a = interner.intern("sys-apps/portage");
        let b = interner.intern("dev-lang/python");
        assert_ne!(a, b);
    }

    #[test]
    fn resolve_returns_original() {
        let interner = Interner::new();
        let sym = interner.intern("app-editors/neovim");
        assert_eq!(interner.resolve(sym).as_deref(), Some("app-editors/neovim"));
    }
}
