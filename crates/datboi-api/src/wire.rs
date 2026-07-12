//! Wire-mode wrapper types: a field's presence/nullability behavior is
//! its TYPE, so it cannot be half-declared the way a hand-paired
//! serde+utoipa annotation could (D69). The /v1 field modes:
//!
//! * `T` — required, never null. Request structs use this for
//!   contractually-required fields too: the server's `ApiJson`
//!   extractor turns serde's missing-field error into the typed 400,
//!   so no Option-for-400 convention remains.
//! * [`Nullable<T>`] — always present, `T | null`. The schema marks
//!   the field required by construction: utoipa exempts only literal
//!   `Option` fields from `required`, and this is not one; pairing it
//!   with `skip_serializing_if` does not compile (no such predicate).
//! * `Option<T>` + `#[serde(skip_serializing_if = "Option::is_none")]`
//!   — optional on the wire, absent when none. Serde cannot omit a
//!   field from inside the value's own `Serialize` impl, so this mode
//!   keeps its one annotation; forgetting it is harmless to consumers
//!   (the generated `field?: T | null` admits the stray null), unlike
//!   the pairs the wrappers replace.
//! * `Option<Nullable<T>>` (+ skip) — absent vs null distinguished.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa::openapi::schema::{ObjectBuilder, OneOfBuilder, Type};
use utoipa::openapi::{Ref, RefOr, Schema};

/// Always present on the wire, `T | null`, required in the schema.
/// `Some` serializes as the value, `None` as `null` — never skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Nullable<T>(pub Option<T>);

impl<T> Nullable<T> {
    #[must_use]
    pub fn into_inner(self) -> Option<T> {
        self.0
    }
}

/// `Nullable` fields read like the Option they carry.
impl<T> std::ops::Deref for Nullable<T> {
    type Target = Option<T>;
    fn deref(&self) -> &Option<T> {
        &self.0
    }
}

impl<T> From<Option<T>> for Nullable<T> {
    fn from(value: Option<T>) -> Self {
        Self(value)
    }
}

impl<T> From<T> for Nullable<T> {
    fn from(value: T) -> Self {
        Self(Some(value))
    }
}

impl<T: Serialize> Serialize for Nullable<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Nullable<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::deserialize(deserializer).map(Self)
    }
}

/// The names utoipa's own `ToSchema` impls give Rust primitives (the
/// `impl_to_schema!` list in utoipa's lib.rs). Primitive inners inline
/// their schema; every other inner is a component `$ref` — the same
/// split utoipa's derive makes for the type in a non-generic position.
fn schema_is_inline(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "bool"
            | "f32"
            | "f64"
            | "String"
            | "str"
            | "char"
    )
}

fn inner_schema<T: ToSchema>() -> RefOr<Schema> {
    if schema_is_inline(&T::name()) {
        T::schema()
    } else {
        Ref::from_schema_name(T::name()).into()
    }
}

// The derive names generic field types `Wrapper_Inner` and builds their
// component schema through this __dev seam; the checked-in-spec test
// pins the rendering byte-for-byte, so a utoipa upgrade that moves the
// seam breaks loudly, not silently.
impl<T: ToSchema> utoipa::__dev::ComposeSchema for Nullable<T> {
    fn compose(_generics: Vec<RefOr<Schema>>) -> RefOr<Schema> {
        OneOfBuilder::new()
            .item(ObjectBuilder::new().schema_type(Type::Null))
            .item(inner_schema::<T>())
            .into()
    }
}

impl<T: ToSchema> ToSchema for Nullable<T> {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("Nullable")
    }

    fn schemas(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        // Referenced inners must exist as components; utoipa's derive
        // only registers what IT referenced, so the wrapper registers
        // its own.
        if !schema_is_inline(&T::name()) {
            schemas.push((T::name().into_owned(), T::schema()));
        }
        T::schemas(schemas);
    }
}

#[cfg(test)]
mod tests {
    use utoipa::PartialSchema;

    use super::*;

    #[test]
    fn nullable_serializes_some_and_null_and_never_skips() {
        assert_eq!(
            serde_json::to_string(&Nullable(Some(7u64))).expect("json"),
            "7"
        );
        assert_eq!(
            serde_json::to_string(&Nullable::<u64>(None)).expect("json"),
            "null"
        );
        let round: Nullable<u64> = serde_json::from_str("null").expect("json");
        assert_eq!(round, Nullable(None));
        let round: Nullable<u64> = serde_json::from_str("7").expect("json");
        assert_eq!(round, Nullable(Some(7)));
    }

    #[test]
    fn nullable_schema_is_inner_or_null() {
        // Primitive inner: inlined.
        let json = serde_json::to_value(<Nullable<u64> as PartialSchema>::schema()).expect("json");
        assert_eq!(json["oneOf"][0]["type"], "null");
        assert_eq!(json["oneOf"][1]["type"], "integer");
        // Contract-type inner: referenced, and registered as a component.
        let json = serde_json::to_value(<Nullable<crate::Revision> as PartialSchema>::schema())
            .expect("json");
        assert_eq!(json["oneOf"][1]["$ref"], "#/components/schemas/Revision");
        let mut components = Vec::new();
        <Nullable<crate::Revision> as ToSchema>::schemas(&mut components);
        assert!(components.iter().any(|(name, _)| name == "Revision"));
    }
}
