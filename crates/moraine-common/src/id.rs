//! Newtype identifier helper.
//!
//! Many crates need compact `u32`-backed identifiers for packages, atoms, and
//! graph nodes. [`define_id`] generates one with the common conversions so each
//! crate does not re-derive the boilerplate.

/// Define a `u32`-backed newtype identifier with raw conversions.
///
/// ```
/// moraine_common::define_id! {
///     /// Identifies a package within a store.
///     PackageId
/// }
/// let id = PackageId::from_raw(7);
/// assert_eq!(id.raw(), 7);
/// ```
#[macro_export]
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
        pub struct $name(u32);

        impl $name {
            /// Construct the identifier from a raw index.
            pub const fn from_raw(raw: u32) -> Self {
                Self(raw)
            }

            /// The raw index backing this identifier.
            pub const fn raw(self) -> u32 {
                self.0
            }
        }
    };
}

#[cfg(test)]
mod tests {
    define_id! {
        /// A test identifier.
        TestId
    }

    #[test]
    fn raw_roundtrip() {
        let id = TestId::from_raw(42);
        assert_eq!(id.raw(), 42);
        assert_eq!(id, TestId::from_raw(42));
        assert_ne!(id, TestId::from_raw(43));
    }
}
