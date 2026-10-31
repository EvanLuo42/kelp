//! Content hashes and changelist identifiers.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A 32-byte BLAKE3 digest.
///
/// [`Debug`](fmt::Debug) prints an abbreviated 16-character hex prefix, while
/// [`Display`](fmt::Display) prints all 64 hex characters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// Computes the BLAKE3 hash of `data`.
    ///
    /// # Examples
    ///
    /// ```
    /// use kelp_core::Hash;
    ///
    /// assert_eq!(Hash::of(b"kelp"), Hash::of(b"kelp"));
    /// assert_ne!(Hash::of(b"kelp"), Hash::of(b"Kelp"));
    /// ```
    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    /// Returns the digest as a 64-character lowercase hex string.
    pub fn to_hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.to_hex()[..16])
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

macro_rules! hash_newtype {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub Hash);

        impl $name {
            /// Computes the hash of `data`.
            pub fn of(data: &[u8]) -> Self {
                Self(Hash::of(data))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

hash_newtype!(
    /// The hash of a single uncompressed chunk.
    ChunkId
);
hash_newtype!(
    /// The hash of a whole file's content.
    ///
    /// It is independent of how the file is chunked.
    ContentId
);
hash_newtype!(
    /// The hash of a canonically serialized tree node.
    TreeId
);

/// A submitted (or shelved) changelist number, monotonically increasing per cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClNumber(pub u64);

impl fmt::Display for ClNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A pending changelist that only exists in one workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LocalClId(pub u32);

impl LocalClId {
    /// The default changelist, which every workspace has.
    pub const DEFAULT: Self = Self(0);
}

impl fmt::Display for LocalClId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::DEFAULT {
            f.write_str("default")
        } else {
            write!(f, "L{}", self.0)
        }
    }
}
