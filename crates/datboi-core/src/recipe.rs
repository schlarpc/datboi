//! Recipe objects (docs/recipes.md): exactly one operation application,
//! `datboi/recipe/1\n` + strict canonical CBOR, identity = blake3 of the
//! whole blob including the prefix (D18).
//!
//! Field keys (CDDL from the design record): recipe {1: op, 2: inputs,
//! 3: outputs, ?4: params}; op {1: "b"|"w", then builtin {2: name,
//! 3: major} or wasm {2: component, 3: world, 4: export}}; input {1: hash,
//! ?2: role}; output {1: hash, 2: size, ?3: name}.
//!
//! One-encoding-per-value rules (strictest reading of "encoder rejects
//! non-canonical"): optional fields are omitted when absent and rejected
//! when present-but-empty (empty params bstr, empty role/name text).

use crate::cbor::{self, Value};
use crate::hash::Blake3;
use crate::object::{self, ObjectKind};

pub const RECIPE_VERSION: u32 = 1;
const HEADER: &[u8] = b"datboi/recipe/1\n";

const KEY_OP: u64 = 1;
const KEY_INPUTS: u64 = 2;
const KEY_OUTPUTS: u64 = 3;
const KEY_PARAMS: u64 = 4;

const OPKEY_KIND: u64 = 1;
const OPKEY_NAME_OR_COMPONENT: u64 = 2;
const OPKEY_MAJOR_OR_WORLD: u64 = 3;
const OPKEY_EXPORT: u64 = 4;

const REFKEY_HASH: u64 = 1;
const INKEY_ROLE: u64 = 2;
const OUTKEY_SIZE: u64 = 2;
const OUTKEY_NAME: u64 = 3;

/// The wasm worlds recipes can name. The wire format stays a string
/// (the canonical spellings below); parsing happens at decode so the
/// executor dispatches on a closed enum, never on string prefixes —
/// "datboi:transform@10" must not look like "@1" and get driven with
/// the wrong ABI. Every other spelling decodes to [`World::Other`]
/// verbatim: a recipe from the future must stay *refusable*
/// (unsupported-op, retryable after an upgrade), never *poisonable*,
/// so unknown worlds survive decode and re-encode byte-identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum World {
    /// `datboi:transform@1` — the streaming transform lane (D89 epoch;
    /// the pre-break whole-buffer @1 and streaming @2 died with the
    /// dev-store wipe, docs/worlds.md).
    Transform1,
    /// `datboi:extractor@1` — the container→member extractor lane.
    Extractor1,
    /// Not a world this build executes; preserved verbatim. Includes
    /// the dead pre-D89 spellings ("datboi:transform@2") — refusable,
    /// never poisonable, like any recipe from the future.
    Other(String),
}

impl World {
    /// Parse a wire spelling. Total on purpose: only the exact
    /// canonical spellings become known worlds ("datboi:transform@1.0.0"
    /// is a different recipe identity and a refusable one, not an
    /// alias), everything else is [`World::Other`].
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "datboi:transform@1" => Self::Transform1,
            "datboi:extractor@1" => Self::Extractor1,
            _ => Self::Other(s.to_owned()),
        }
    }

    /// The single export a world sanctions, when the world fixes it:
    /// the extractor world has exactly one entry point, so any other
    /// export string is a different recipe identity for the same
    /// computation. Transform worlds export op families chosen per
    /// recipe (None). Mint and dispatch both read this — one home.
    #[must_use]
    pub fn required_export(&self) -> Option<&'static str> {
        match self {
            Self::Extractor1 => Some("extract"),
            _ => None,
        }
    }

    /// The canonical wire spelling — the ONE string mint sites and the
    /// encoder emit for a known world, so spelling variants can never
    /// fragment recipe identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Transform1 => "datboi:transform@1",
            Self::Extractor1 => "datboi:extractor@1",
            Self::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Builtin {
        name: String,
        major: u32,
    },
    Wasm {
        component: Blake3,
        world: World,
        export: String,
    },
}

impl Op {
    /// The ONE queryable spelling of an op — builtin `name@major`, wasm
    /// `<component-hex>#<export>`. Every recipe index row derives its
    /// op_name here, so display grammars (the RouteEdge.op vocabulary)
    /// can never meet a second spelling of the same logical op.
    #[must_use]
    pub fn index_name(&self) -> String {
        match self {
            Self::Builtin { name, major } => format!("{name}@{major}"),
            Self::Wasm {
                component, export, ..
            } => format!("{}#{export}", component.to_hex()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRef {
    pub hash: Blake3,
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRef {
    pub hash: Blake3,
    pub size: u64,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub op: Op,
    pub inputs: Vec<InputRef>,
    pub outputs: Vec<OutputRef>,
    /// Canonical-CBOR params bstr, schema owned by the op. Empty = absent.
    pub params: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecipeError {
    #[error("not a datboi recipe object")]
    NotARecipe,
    #[error("unsupported recipe version {0}")]
    Version(u32),
    #[error(transparent)]
    Cbor(#[from] cbor::DecodeError),
    #[error("recipe must claim at least one output")]
    NoOutputs,
    #[error("invalid recipe structure: {0}")]
    Invalid(&'static str),
}

impl Recipe {
    /// Encode to the canonical object bytes (header + CBOR). The returned
    /// bytes ARE the recipe's identity: blake3(encode()) never changes for
    /// a given logical recipe.
    pub fn encode(&self) -> Result<Vec<u8>, RecipeError> {
        self.validate()?;
        let mut entries = vec![
            (KEY_OP, op_to_value(&self.op)),
            (
                KEY_INPUTS,
                Value::Array(self.inputs.iter().map(input_to_value).collect()),
            ),
            (
                KEY_OUTPUTS,
                Value::Array(self.outputs.iter().map(output_to_value).collect()),
            ),
        ];
        if !self.params.is_empty() {
            entries.push((KEY_PARAMS, Value::Bytes(self.params.clone())));
        }
        let body = cbor::encode(&Value::Map(entries)).expect("field keys are distinct constants");
        let mut out = Vec::with_capacity(HEADER.len() + body.len());
        out.extend_from_slice(HEADER);
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, RecipeError> {
        let (kind, version, body) = object::sniff(bytes).ok_or(RecipeError::NotARecipe)?;
        if kind != ObjectKind::Recipe {
            return Err(RecipeError::NotARecipe);
        }
        if version != RECIPE_VERSION {
            return Err(RecipeError::Version(version));
        }
        let value = cbor::decode(&bytes[body..])?;
        let entries = as_map(&value)?;
        let mut op = None;
        let mut inputs = None;
        let mut outputs = None;
        let mut params = Vec::new();
        for (key, val) in entries {
            match *key {
                KEY_OP => op = Some(op_from_value(val)?),
                KEY_INPUTS => inputs = Some(refs_from_value(val, input_from_value)?),
                KEY_OUTPUTS => outputs = Some(refs_from_value(val, output_from_value)?),
                KEY_PARAMS => {
                    let bytes = as_bytes(val)?;
                    if bytes.is_empty() {
                        return Err(RecipeError::Invalid("params present but empty"));
                    }
                    params = bytes.to_vec();
                }
                _ => return Err(RecipeError::Invalid("unknown recipe key")),
            }
        }
        let recipe = Self {
            op: op.ok_or(RecipeError::Invalid("missing op"))?,
            inputs: inputs.ok_or(RecipeError::Invalid("missing inputs"))?,
            outputs: outputs.ok_or(RecipeError::Invalid("missing outputs"))?,
            params,
        };
        recipe.validate()?;
        Ok(recipe)
    }

    fn validate(&self) -> Result<(), RecipeError> {
        if self.outputs.is_empty() {
            return Err(RecipeError::NoOutputs);
        }
        let optional_texts = self
            .inputs
            .iter()
            .map(|i| &i.role)
            .chain(self.outputs.iter().map(|o| &o.name));
        for text in optional_texts.flatten() {
            if text.is_empty() {
                return Err(RecipeError::Invalid("empty optional text field"));
            }
        }
        if let Op::Builtin { name, .. } = &self.op
            && name.is_empty()
        {
            return Err(RecipeError::Invalid("empty builtin name"));
        }
        if let Op::Wasm { world, export, .. } = &self.op
            && (world.as_str().is_empty() || export.is_empty())
        {
            return Err(RecipeError::Invalid("empty wasm world or export"));
        }
        Ok(())
    }
}

fn op_to_value(op: &Op) -> Value {
    match op {
        Op::Builtin { name, major } => Value::Map(vec![
            (OPKEY_KIND, Value::Text("b".into())),
            (OPKEY_NAME_OR_COMPONENT, Value::Text(name.clone())),
            (OPKEY_MAJOR_OR_WORLD, Value::Uint(u64::from(*major))),
        ]),
        Op::Wasm {
            component,
            world,
            export,
        } => Value::Map(vec![
            (OPKEY_KIND, Value::Text("w".into())),
            (OPKEY_NAME_OR_COMPONENT, Value::Bytes(component.0.to_vec())),
            (OPKEY_MAJOR_OR_WORLD, Value::Text(world.as_str().to_owned())),
            (OPKEY_EXPORT, Value::Text(export.clone())),
        ]),
    }
}

fn input_to_value(input: &InputRef) -> Value {
    let mut entries = vec![(REFKEY_HASH, Value::Bytes(input.hash.0.to_vec()))];
    if let Some(role) = &input.role {
        entries.push((INKEY_ROLE, Value::Text(role.clone())));
    }
    Value::Map(entries)
}

fn output_to_value(output: &OutputRef) -> Value {
    let mut entries = vec![
        (REFKEY_HASH, Value::Bytes(output.hash.0.to_vec())),
        (OUTKEY_SIZE, Value::Uint(output.size)),
    ];
    if let Some(name) = &output.name {
        entries.push((OUTKEY_NAME, Value::Text(name.clone())));
    }
    Value::Map(entries)
}

fn op_from_value(value: &Value) -> Result<Op, RecipeError> {
    let entries = as_map(value)?;
    let kind = entries
        .iter()
        .find(|(k, _)| *k == OPKEY_KIND)
        .ok_or(RecipeError::Invalid("op missing kind"))?;
    match as_text(&kind.1)? {
        "b" => {
            let (mut name, mut major) = (None, None);
            for (key, val) in entries {
                match *key {
                    OPKEY_KIND => {}
                    OPKEY_NAME_OR_COMPONENT => name = Some(as_text(val)?.to_owned()),
                    OPKEY_MAJOR_OR_WORLD => {
                        let raw = as_uint(val)?;
                        major = Some(
                            u32::try_from(raw)
                                .map_err(|_| RecipeError::Invalid("builtin major out of range"))?,
                        );
                    }
                    _ => return Err(RecipeError::Invalid("unknown builtin op key")),
                }
            }
            Ok(Op::Builtin {
                name: name.ok_or(RecipeError::Invalid("builtin missing name"))?,
                major: major.ok_or(RecipeError::Invalid("builtin missing major"))?,
            })
        }
        "w" => {
            let (mut component, mut world, mut export) = (None, None, None);
            for (key, val) in entries {
                match *key {
                    OPKEY_KIND => {}
                    OPKEY_NAME_OR_COMPONENT => component = Some(as_hash(val)?),
                    OPKEY_MAJOR_OR_WORLD => world = Some(World::parse(as_text(val)?)),
                    OPKEY_EXPORT => export = Some(as_text(val)?.to_owned()),
                    _ => return Err(RecipeError::Invalid("unknown wasm op key")),
                }
            }
            Ok(Op::Wasm {
                component: component.ok_or(RecipeError::Invalid("wasm missing component"))?,
                world: world.ok_or(RecipeError::Invalid("wasm missing world"))?,
                export: export.ok_or(RecipeError::Invalid("wasm missing export"))?,
            })
        }
        _ => Err(RecipeError::Invalid("unknown op kind")),
    }
}

fn refs_from_value<T>(
    value: &Value,
    parse: impl Fn(&Value) -> Result<T, RecipeError>,
) -> Result<Vec<T>, RecipeError> {
    let Value::Array(items) = value else {
        return Err(RecipeError::Invalid("expected array"));
    };
    items.iter().map(parse).collect()
}

fn input_from_value(value: &Value) -> Result<InputRef, RecipeError> {
    let (mut hash, mut role) = (None, None);
    for (key, val) in as_map(value)? {
        match *key {
            REFKEY_HASH => hash = Some(as_hash(val)?),
            INKEY_ROLE => role = Some(as_text(val)?.to_owned()),
            _ => return Err(RecipeError::Invalid("unknown input key")),
        }
    }
    Ok(InputRef {
        hash: hash.ok_or(RecipeError::Invalid("input missing hash"))?,
        role,
    })
}

fn output_from_value(value: &Value) -> Result<OutputRef, RecipeError> {
    let (mut hash, mut size, mut name) = (None, None, None);
    for (key, val) in as_map(value)? {
        match *key {
            REFKEY_HASH => hash = Some(as_hash(val)?),
            OUTKEY_SIZE => size = Some(as_uint(val)?),
            OUTKEY_NAME => name = Some(as_text(val)?.to_owned()),
            _ => return Err(RecipeError::Invalid("unknown output key")),
        }
    }
    Ok(OutputRef {
        hash: hash.ok_or(RecipeError::Invalid("output missing hash"))?,
        size: size.ok_or(RecipeError::Invalid("output missing size"))?,
        name,
    })
}

fn as_map(value: &Value) -> Result<&[(u64, Value)], RecipeError> {
    match value {
        Value::Map(entries) => Ok(entries),
        _ => Err(RecipeError::Invalid("expected map")),
    }
}

fn as_text(value: &Value) -> Result<&str, RecipeError> {
    match value {
        Value::Text(t) => Ok(t),
        _ => Err(RecipeError::Invalid("expected text")),
    }
}

fn as_uint(value: &Value) -> Result<u64, RecipeError> {
    match value {
        Value::Uint(n) => Ok(*n),
        _ => Err(RecipeError::Invalid("expected unsigned integer")),
    }
}

fn as_bytes(value: &Value) -> Result<&[u8], RecipeError> {
    match value {
        Value::Bytes(b) => Ok(b),
        _ => Err(RecipeError::Invalid("expected byte string")),
    }
}

fn as_hash(value: &Value) -> Result<Blake3, RecipeError> {
    let bytes = as_bytes(value)?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| RecipeError::Invalid("hash must be exactly 32 bytes"))?;
    Ok(Blake3(arr))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn golden_recipe() -> Recipe {
        Recipe {
            op: Op::Builtin {
                name: "assemble".into(),
                major: 1,
            },
            inputs: vec![
                InputRef {
                    hash: Blake3::compute(b"header"),
                    role: None,
                },
                InputRef {
                    hash: Blake3::compute(b"body"),
                    role: Some("body".into()),
                },
            ],
            outputs: vec![OutputRef {
                hash: Blake3::compute(b"rom"),
                size: 524_304,
                name: Some("Game.nes".into()),
            }],
            params: vec![0x80],
        }
    }

    /// FORMAT COMMITMENT: this hash is the recipe wire format. If this test
    /// ever fails, the format changed and every existing recipe identity
    /// breaks — that is never acceptable after M1 freezes the codec (D5).
    #[test]
    fn golden_vector_identity() {
        let encoded = golden_recipe().encode().expect("valid recipe");
        assert!(encoded.starts_with(b"datboi/recipe/1\n"));
        assert_eq!(
            Blake3::compute(&encoded).to_hex(),
            "3bfdd74c2578d41f300540c2220ef31737bec43dc9eedad59ccae069b7c5e4ec"
        );
    }

    #[test]
    fn world_parses_exact_canonical_spellings_only() {
        for w in [World::Transform1, World::Extractor1] {
            assert_eq!(World::parse(w.as_str()), w, "canonical round-trip");
        }
        // Prefix relatives and semver spellings are NOT the frozen
        // worlds: they must decode (refusable), never alias an ABI.
        for s in [
            "datboi:transform@10",
            "datboi:transform@2.0.0",
            "datboi:extractor@11",
            "datboi:transform@2 ",
        ] {
            assert_eq!(World::parse(s), World::Other(s.to_owned()));
            assert_eq!(World::parse(s).as_str(), s, "verbatim survival");
        }
        // The extractor world fixes its one export; transform worlds
        // pick exports per recipe.
        assert_eq!(World::Extractor1.required_export(), Some("extract"));
        assert_eq!(World::Transform1.required_export(), None);
        assert_eq!(World::Transform1.required_export(), None);
    }

    #[test]
    fn index_name_spells_the_route_grammar() {
        assert_eq!(golden_recipe().op.index_name(), "assemble@1");
        let component = Blake3::compute(b"component");
        let wasm = Op::Wasm {
            component,
            world: World::Extractor1,
            export: "extract".into(),
        };
        assert_eq!(wasm.index_name(), format!("{}#extract", component.to_hex()));
    }

    #[test]
    fn round_trips() {
        let recipe = golden_recipe();
        let encoded = recipe.encode().expect("valid recipe");
        assert_eq!(Recipe::decode(&encoded).expect("decodes"), recipe);
    }

    #[test]
    fn rejects_structural_violations() {
        let mut no_outputs = golden_recipe();
        no_outputs.outputs.clear();
        assert_eq!(no_outputs.encode(), Err(RecipeError::NoOutputs));

        let mut empty_role = golden_recipe();
        empty_role.inputs[1].role = Some(String::new());
        assert_eq!(
            empty_role.encode(),
            Err(RecipeError::Invalid("empty optional text field"))
        );

        assert_eq!(
            Recipe::decode(b"NES\x1a rubbish"),
            Err(RecipeError::NotARecipe)
        );
        assert_eq!(
            Recipe::decode(b"datboi/recipe/2\n\xa0"),
            Err(RecipeError::Version(2))
        );
        // Wrong object kind with valid header shape.
        assert_eq!(
            Recipe::decode(b"datboi/viewsnap/1\n\xa0"),
            Err(RecipeError::NotARecipe)
        );
    }

    fn hash_strategy() -> impl Strategy<Value = Blake3> {
        any::<[u8; 32]>().prop_map(Blake3)
    }

    fn opt_text() -> impl Strategy<Value = Option<String>> {
        prop::option::of("[a-z]{1,12}")
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            ("[a-z-]{1,16}", any::<u32>()).prop_map(|(name, major)| Op::Builtin { name, major }),
            (hash_strategy(), "[a-z:@0-9.]{1,24}", "[a-z-]{1,16}").prop_map(
                |(component, world, export)| Op::Wasm {
                    component,
                    world: World::parse(&world),
                    export
                }
            ),
        ]
    }

    fn recipe_strategy() -> impl Strategy<Value = Recipe> {
        (
            op_strategy(),
            prop::collection::vec(
                (hash_strategy(), opt_text()).prop_map(|(hash, role)| InputRef { hash, role }),
                0..4,
            ),
            prop::collection::vec(
                (hash_strategy(), any::<u64>(), opt_text())
                    .prop_map(|(hash, size, name)| OutputRef { hash, size, name }),
                1..4,
            ),
            prop::collection::vec(any::<u8>(), 0..64),
        )
            .prop_map(|(op, inputs, outputs, params)| Recipe {
                op,
                inputs,
                outputs,
                params,
            })
    }

    proptest! {
        #[test]
        fn round_trip_property(recipe in recipe_strategy()) {
            let first = recipe.encode().expect("valid recipe");
            let decoded = Recipe::decode(&first).expect("decodes");
            prop_assert_eq!(&decoded, &recipe);
            prop_assert_eq!(decoded.encode().expect("re-encodes"), first);
        }
    }
}
