//! Out-of-band recipe selection.
//!
//! `recipe_writer::dependent` already selects a schema *inside* the bytes: a
//! `switch` reads a field it just parsed and picks a case. This crate is the
//! outer twin of that switch. The key is not in the bytes: it is a compact
//! bitpacked word (`family`, `version`, `variant`) carried out of band by the
//! transport, and the second selector is the one thing every transport knows
//! before parsing, the observed length. `(key, length)` picks exactly one
//! [`DependentRecipe`]; the recipe then does what it always did.
//!
//! The catalog is built once and refuses ambiguity at build time: two recipes
//! under one key may not claim overlapping lengths at the same specificity, so
//! resolution is deterministic and needs no priority list.
//!
//! The lookup is a map behind [`RecipeRegistry`]'s methods. If profiling ever
//! says the map is hot, that is the seam where a pointer-dispatch table goes;
//! nothing outside this crate sees the difference.

use std::collections::BTreeMap;
use std::fmt;

use big_bad_bool::{bitpack, PackedWord};
use recipe::error::RecipeError;
use recipe_writer::dependent::{DependentParseResult, DependentRecipe};
use thiserror::Error;

/// The out-of-band key: which family of recipes, which version of that family,
/// which variant. Packed into one `u32` so a transport carries it in a header
/// field and a catalog keys on the raw word.
///
/// ```text
/// bits  0..8   family
/// bits  8..16  version
/// bits 16..24  variant
/// bits 24..32  reserved (must be zero)
/// ```
///
/// The `#[bitpack]` macro generates the field accessors, `new`, `try_from_raw`
/// and the `PackedWord` impl; `word` / `from_word` are the transport-facing pair.
#[bitpack(storage = u32)]
pub struct RecipeKey {
    #[bits(8)]
    family: u32,

    #[bits(8)]
    version: u32,

    #[bits(8)]
    variant: u32,

    #[bits(8)]
    reserved: u32,
}

impl RecipeKey {
    /// A key from its three parts.
    pub const fn of(family: u8, version: u8, variant: u8) -> Self {
        Self::new(family as u32, version as u32, variant as u32, 0)
    }

    /// The packed word, the form a transport carries.
    pub fn word(self) -> u32 {
        self.raw_storage()
    }

    /// A key from a packed word; `None` when the reserved bits are set.
    pub fn from_word(raw: u32) -> Option<Self> {
        let key = Self::try_from_raw(raw)?;
        if key.reserved() != 0 {
            return None;
        }
        Some(key)
    }
}

impl fmt::Display for RecipeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.family(), self.version(), self.variant())
    }
}

impl PartialOrd for RecipeKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RecipeKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.word().cmp(&other.word())
    }
}

/// What a recipe claims about the total length of the bytes it parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthConstraint {
    /// Exactly this many bytes.
    Exact(usize),
    /// Between `min` and `max` bytes, both inclusive.
    Range { min: usize, max: usize },
    /// At least this many bytes.
    AtLeast(usize),
}

impl LengthConstraint {
    /// Whether `len` satisfies the constraint.
    pub fn matches(self, len: usize) -> bool {
        match self {
            Self::Exact(n) => len == n,
            Self::Range { min, max } => (min..=max).contains(&len),
            Self::AtLeast(n) => len >= n,
        }
    }

    fn bounds(self) -> (usize, usize) {
        match self {
            Self::Exact(n) => (n, n),
            Self::Range { min, max } => (min, max),
            Self::AtLeast(n) => (n, usize::MAX),
        }
    }

    /// Whether two constraints can both match some length.
    pub fn overlaps(self, other: Self) -> bool {
        let (a0, a1) = self.bounds();
        let (b0, b1) = other.bounds();
        a0 <= b1 && b0 <= a1
    }

    /// Lower is more specific: an exact length beats a range beats a floor.
    fn specificity(self) -> u8 {
        match self {
            Self::Exact(_) => 0,
            Self::Range { .. } => 1,
            Self::AtLeast(_) => 2,
        }
    }

    fn width(self) -> usize {
        let (lo, hi) = self.bounds();
        hi - lo
    }

    fn well_formed(self) -> bool {
        match self {
            Self::Range { min, max } => min <= max,
            _ => true,
        }
    }
}

impl fmt::Display for LengthConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(n) => write!(f, "exactly {n}"),
            Self::Range { min, max } => write!(f, "{min}..={max}"),
            Self::AtLeast(n) => write!(f, "at least {n}"),
        }
    }
}

/// One registered recipe: the key it answers to, the lengths it claims, the
/// recipe itself.
#[derive(Debug, Clone)]
pub struct RegisteredRecipe {
    pub key: RecipeKey,
    pub constraint: LengthConstraint,
    pub recipe: DependentRecipe,
}

impl RegisteredRecipe {
    /// The recipe's own name.
    pub fn name(&self) -> &str {
        &self.recipe.name
    }
}

/// Why the catalog refused a registration or a lookup found nothing.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// A `Range` with `min > max`.
    #[error("length constraint {constraint} is malformed")]
    MalformedConstraint { constraint: LengthConstraint },
    /// Under one key, two recipes of the same specificity claim a common length.
    #[error("key {key}: `{new}` ({new_constraint}) overlaps `{existing}` ({existing_constraint})")]
    Ambiguous {
        key: RecipeKey,
        existing: String,
        existing_constraint: LengthConstraint,
        new: String,
        new_constraint: LengthConstraint,
    },
    /// The same recipe name registered twice under one key.
    #[error("key {key}: `{name}` is already registered")]
    Duplicate { key: RecipeKey, name: String },
    /// No recipe answers to the key at all.
    #[error("no recipe registered under key {key}")]
    UnknownKey { key: RecipeKey },
    /// Recipes exist for the key, none claims this length.
    #[error("key {key}: no variant claims {len} bytes (registered: {claims})")]
    NoVariantForLength { key: RecipeKey, len: usize, claims: String },
    /// The selected recipe rejected the bytes.
    #[error("key {key} variant `{variant}`: {source}")]
    Parse {
        key: RecipeKey,
        variant: String,
        #[source]
        source: RecipeError,
    },
}

/// A keyed parse: which variant answered and what it produced.
#[derive(Debug)]
pub struct KeyedParse {
    pub key: RecipeKey,
    pub variant: String,
    pub constraint: LengthConstraint,
    pub result: DependentParseResult,
}

/// The catalog: raw key word → the recipes registered under it.
#[derive(Debug, Default, Clone)]
pub struct RecipeRegistry {
    entries: BTreeMap<u32, Vec<RegisteredRecipe>>,
}

impl RecipeRegistry {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `recipe` under `key` for lengths satisfying `constraint`.
    ///
    /// Refused when the constraint is malformed, when the recipe name is
    /// already registered under the key, or when another recipe of the same
    /// specificity under the key overlaps it. An `Exact` may sit inside a
    /// `Range` or an `AtLeast`, and a `Range` inside an `AtLeast`: the more
    /// specific one wins at lookup, so that is not ambiguous.
    pub fn register(
        &mut self,
        key: RecipeKey,
        constraint: LengthConstraint,
        recipe: DependentRecipe,
    ) -> Result<(), RegistryError> {
        if !constraint.well_formed() {
            return Err(RegistryError::MalformedConstraint { constraint });
        }
        let entries = self.entries.entry(key.word()).or_default();
        for existing in entries.iter() {
            if existing.recipe.name == recipe.name {
                return Err(RegistryError::Duplicate { key, name: recipe.name });
            }
            if existing.constraint.specificity() == constraint.specificity()
                && existing.constraint.overlaps(constraint)
            {
                return Err(RegistryError::Ambiguous {
                    key,
                    existing: existing.recipe.name.clone(),
                    existing_constraint: existing.constraint,
                    new: recipe.name,
                    new_constraint: constraint,
                });
            }
        }
        entries.push(RegisteredRecipe { key, constraint, recipe });
        Ok(())
    }

    /// The registered recipe for `key` and an observed total length: the most
    /// specific claim that matches, narrowest first.
    pub fn resolve(&self, key: RecipeKey, len: usize) -> Result<&RegisteredRecipe, RegistryError> {
        let entries = self.entries.get(&key.word()).ok_or(RegistryError::UnknownKey { key })?;
        entries
            .iter()
            .filter(|e| e.constraint.matches(len))
            .min_by_key(|e| (e.constraint.specificity(), e.constraint.width()))
            .ok_or_else(|| RegistryError::NoVariantForLength {
                key,
                len,
                claims: entries
                    .iter()
                    .map(|e| format!("`{}` {}", e.recipe.name, e.constraint))
                    .collect::<Vec<_>>()
                    .join(", "),
            })
    }

    /// Select by `(key, data.len())` and parse.
    pub fn parse_keyed(&self, key: RecipeKey, data: &[u8]) -> Result<KeyedParse, RegistryError> {
        let entry = self.resolve(key, data.len())?;
        let result = entry.recipe.parse(data).map_err(|source| RegistryError::Parse {
            key,
            variant: entry.recipe.name.clone(),
            source,
        })?;
        Ok(KeyedParse {
            key,
            variant: entry.recipe.name.clone(),
            constraint: entry.constraint,
            result,
        })
    }

    /// Select by a raw key word (as a transport carries it) and parse.
    pub fn parse_raw(&self, raw: u32, data: &[u8]) -> Result<KeyedParse, RegistryError> {
        let key = RecipeKey::from_word(raw).ok_or_else(|| RegistryError::UnknownKey {
            key: RecipeKey::from_raw_u128_truncating(raw as u128),
        })?;
        self.parse_keyed(key, data)
    }

    /// Every recipe registered under `key`, in registration order.
    pub fn variants(&self, key: RecipeKey) -> &[RegisteredRecipe] {
        self.entries.get(&key.word()).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every key with at least one recipe, ascending.
    pub fn keys(&self) -> impl Iterator<Item = RecipeKey> + '_ {
        self.entries.keys().filter_map(|raw| RecipeKey::from_word(*raw))
    }

    /// Number of registered recipes across all keys.
    pub fn len(&self) -> usize {
        self.entries.values().map(Vec::len).sum()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
