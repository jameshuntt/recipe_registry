use recipe::ingredient_value::IngredientValue;
use recipe_registry::{LengthConstraint, RecipeKey, RecipeRegistry, RegistryError};
use recipe_writer::dependent::{DependentField, DependentRecipe, LengthExpr};
use std::collections::HashMap;

fn field(value: &IngredientValue, name: &str) -> IngredientValue {
    match value {
        IngredientValue::Struct(map) => map.get(name).cloned().expect(name),
        other => panic!("expected struct, got {other:?}"),
    }
}

fn get_struct(value: &IngredientValue) -> &HashMap<String, IngredientValue> {
    match value {
        IngredientValue::Struct(map) => map,
        other => panic!("expected struct, got {other:?}"),
    }
}

/// Two varints then a length-prefixed body.
fn framed(name: &str) -> DependentRecipe {
    DependentRecipe::new(name)
        .field(DependentField::quic_varint("kind"))
        .field(DependentField::quic_varint("len"))
        .field(DependentField::bytes("body", LengthExpr::from_field("len")))
}

/// One varint, the rest raw.
fn tagged_rest(name: &str) -> DependentRecipe {
    DependentRecipe::new(name)
        .field(DependentField::quic_varint("tag"))
        .field(DependentField::bytes("rest", LengthExpr::Remaining))
}

const SESSION: RecipeKey = RecipeKey::of(0x17, 1, 0);

#[test]
fn the_key_packs_and_round_trips_through_its_raw_word() {
    let key = RecipeKey::of(0x17, 2, 5);
    assert_eq!(key.family(), 0x17);
    assert_eq!(key.version(), 2);
    assert_eq!(key.variant(), 5);
    assert_eq!(key.word(), 0x17 | (2 << 8) | (5 << 16));
    assert_eq!(RecipeKey::from_word(key.word()), Some(key));
    assert_eq!(key.to_string(), "23.2.5");
    assert_eq!(RecipeKey::from_word(key.word() | 0x0100_0000), None, "reserved bits must be zero");
}

#[test]
fn key_and_observed_length_select_the_variant() {
    let mut reg = RecipeRegistry::new();
    reg.register(SESSION, LengthConstraint::Exact(3), tagged_rest("session_open")).unwrap();
    reg.register(SESSION, LengthConstraint::AtLeast(4), framed("session_data")).unwrap();

    let open = reg.parse_keyed(SESSION, &[0x01, 0xAA, 0xBB]).unwrap();
    assert_eq!(open.variant, "session_open");
    assert_eq!(open.constraint, LengthConstraint::Exact(3));
    assert_eq!(field(&open.result.value, "rest"), IngredientValue::Binary(vec![0xAA, 0xBB]));

    let data = reg.parse_keyed(SESSION, &[0x02, 0x03, 0x10, 0x20, 0x30]).unwrap();
    assert_eq!(data.variant, "session_data");
    assert_eq!(field(&data.result.value, "body"), IngredientValue::Binary(vec![0x10, 0x20, 0x30]));
    assert_eq!(data.result.bytes_consumed, 5);

    let raw = reg.parse_raw(SESSION.word(), &[0x01, 0xAA, 0xBB]).unwrap();
    assert_eq!(raw.variant, "session_open");
}

#[test]
fn the_most_specific_claim_wins_and_the_rest_is_deterministic() {
    let mut reg = RecipeRegistry::new();
    reg.register(SESSION, LengthConstraint::AtLeast(1), tagged_rest("catch_all")).unwrap();
    reg.register(SESSION, LengthConstraint::Range { min: 2, max: 8 }, tagged_rest("small")).unwrap();
    reg.register(SESSION, LengthConstraint::Exact(4), tagged_rest("four")).unwrap();

    assert_eq!(reg.resolve(SESSION, 1).unwrap().name(), "catch_all");
    assert_eq!(reg.resolve(SESSION, 2).unwrap().name(), "small");
    assert_eq!(reg.resolve(SESSION, 4).unwrap().name(), "four");
    assert_eq!(reg.resolve(SESSION, 8).unwrap().name(), "small");
    assert_eq!(reg.resolve(SESSION, 9).unwrap().name(), "catch_all");
    assert_eq!(reg.variants(SESSION).len(), 3);
    assert_eq!(reg.len(), 3);
    assert_eq!(reg.keys().collect::<Vec<_>>(), vec![SESSION]);
}

#[test]
fn ambiguity_is_refused_when_the_catalog_is_built() {
    let mut reg = RecipeRegistry::new();
    reg.register(SESSION, LengthConstraint::Range { min: 2, max: 8 }, tagged_rest("a")).unwrap();

    let err = reg
        .register(SESSION, LengthConstraint::Range { min: 8, max: 10 }, tagged_rest("b"))
        .unwrap_err();
    assert!(matches!(err, RegistryError::Ambiguous { ref existing, ref new, .. } if existing == "a" && new == "b"), "{err}");

    let err = reg.register(SESSION, LengthConstraint::Exact(3), tagged_rest("a")).unwrap_err();
    assert!(matches!(err, RegistryError::Duplicate { .. }), "{err}");

    let err = reg
        .register(SESSION, LengthConstraint::Range { min: 9, max: 3 }, tagged_rest("c"))
        .unwrap_err();
    assert!(matches!(err, RegistryError::MalformedConstraint { .. }), "{err}");

    reg.register(SESSION, LengthConstraint::AtLeast(0), tagged_rest("floor")).unwrap();
    let err = reg.register(SESSION, LengthConstraint::AtLeast(100), tagged_rest("floor2")).unwrap_err();
    assert!(matches!(err, RegistryError::Ambiguous { .. }), "two floors always overlap: {err}");

    assert_eq!(reg.len(), 2, "a refused registration leaves the catalog as it was");
}

#[test]
fn unknown_key_unclaimed_length_and_parse_failures_are_distinct_errors() {
    let mut reg = RecipeRegistry::new();
    reg.register(SESSION, LengthConstraint::Range { min: 3, max: 5 }, framed("session_data")).unwrap();

    let err = reg.parse_keyed(RecipeKey::of(0x18, 1, 0), &[0, 0, 0]).unwrap_err();
    assert!(matches!(err, RegistryError::UnknownKey { .. }), "{err}");

    let err = reg.parse_keyed(SESSION, &[0, 0]).unwrap_err();
    assert!(matches!(err, RegistryError::NoVariantForLength { len: 2, .. }), "{err}");
    assert!(err.to_string().contains("`session_data` 3..=5"), "{err}");

    // len claims 4 body bytes but only 1 follows: the recipe's own bounds check
    let err = reg.parse_keyed(SESSION, &[0x02, 0x04, 0xAA]).unwrap_err();
    assert!(matches!(err, RegistryError::Parse { ref variant, .. } if variant == "session_data"), "{err}");

    let err = reg.parse_raw(SESSION.word() | 0xFF00_0000, &[0, 0, 0]).unwrap_err();
    assert!(matches!(err, RegistryError::UnknownKey { .. }), "reserved bits set: {err}");
}

#[test]
fn a_switch_inside_the_selected_recipe_still_works() {
    use recipe_writer::dependent::SchemaCase;
    use std::collections::BTreeMap;

    let mut cases = BTreeMap::new();
    cases.insert(1u64, SchemaCase::new("one", vec![DependentField::bytes("x", LengthExpr::Remaining)]));
    let recipe = DependentRecipe::new("switched")
        .field(DependentField::quic_varint("tag"))
        .field(DependentField::switch_with_fallback(
            "active",
            "tag",
            cases,
            SchemaCase::new("other", vec![DependentField::bytes("raw", LengthExpr::Remaining)]),
        ));
    let mut reg = RecipeRegistry::new();
    reg.register(SESSION, LengthConstraint::AtLeast(1), recipe).unwrap();

    let one = reg.parse_keyed(SESSION, &[0x01, 0x42]).unwrap();
    let active_value = field(&one.result.value, "active");
    let active = get_struct(&active_value);
    assert_eq!(active.get("x"), Some(&IngredientValue::Binary(vec![0x42])));
    assert_eq!(one.result.transitions[0].schema_name, "one");

    let other = reg.parse_keyed(SESSION, &[0x09, 0x42, 0x43]).unwrap();
    assert_eq!(other.result.transitions[0].schema_name, "other");
}
