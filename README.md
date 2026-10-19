# recipe_registry

Out-of-band recipe selection for [`recipe`](https://crates.io/crates/recipe).

`recipe_writer::dependent` already selects a schema *inside* the bytes: a
`switch` reads a field it just parsed and picks a case. This crate is the outer
twin of that switch. The key is not in the bytes. It is a compact bit-packed
word, `family`, `version`, `variant`, carried out of band by whatever transport
you use, and the second selector is the one thing every transport knows before
parsing: the observed length. `(key, length)` picks exactly one
`DependentRecipe`; the recipe then does what it always did.

The catalog is built once and refuses ambiguity at build time: two recipes
under one key may not claim overlapping lengths at the same specificity, so
resolution is deterministic and needs no priority list.

## Types

`RecipeKey` (24 bits: family, version, variant, packed with
[`big-bad-bool`](https://crates.io/crates/big-bad-bool)), `LengthConstraint`,
`RegisteredRecipe`, `RecipeRegistry`, `KeyedParse`, `RegistryError`.

## Tests

```bash
cargo test
```

## Releasing

Bump `version` in Cargo.toml, add a dated `## <version> (YYYY-MM-DD)` entry to CHANGELOG.md, and push to
`main`. CI (.github/workflows/ci.yml) runs the tests and clippy, then publishes that version to crates.io.

## License

MIT or Apache-2.0, at your option.
