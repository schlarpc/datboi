//! The M1 determinism gate (docs/90-roadmap.md prototype 3, D5).
//!
//! Runs the committed `xf-reference` component — fixed bytes, not rebuilt
//! from source — because that is the production model: recipes pin a
//! component *hash*, so the artifact is the contract, and rebuilding from
//! source would test rustc's reproducibility instead of ours. The golden
//! constants below anchor cross-architecture agreement: this same file runs
//! on x86_64 and aarch64 and must produce identical bytes on both, forever.
//!
//! Updating the fixture is a *format event*: new bytes ⇒ new component hash
//! ⇒ the golden constants below must be re-pinned, exactly as recipes in the
//! wild would have to re-pin the component they reference.

use datboi_runtime::{Limits, RuntimeError, SeekClass, TransformHost};

/// The frozen reference component: `transforms/xf-reference` built for
/// wasm32-unknown-unknown and componentized with `wasm-tools component new`.
const COMPONENT: &[u8] = include_bytes!("fixtures/xf_reference.wasm");

/// blake3 of the fixture itself — the identity a recipe would pin (D5/D6).
const COMPONENT_BLAKE3: &str = "b713cd2671eec4d46489001fe979652359ff325c8f4ebe27cd7974a65eade7b7";

/// blake3 of `run("bitswap", [], [PATTERN])[0]` — the cross-arch anchor.
const BITSWAP_OUTPUT_BLAKE3: &str =
    "baa41f8f9eba49f83b2d1715f4983f93d1b8ed9abe1cb8ac3d6a08dfd5ce4021";

/// 64 KiB of fixed, incompressible-ish input; covers every byte value.
fn pattern() -> Vec<u8> {
    (0..64 * 1024).map(|i| (i * 31 % 251) as u8).collect()
}

fn host() -> TransformHost {
    TransformHost::new(Limits::default()).expect("deterministic config accepted")
}

#[test]
fn component_bytes_are_pinned() {
    assert_eq!(
        blake3::hash(COMPONENT).to_hex().as_str(),
        COMPONENT_BLAKE3,
        "fixture changed: re-pin the golden constants (this is a format event)"
    );
}

#[test]
fn describe_reports_affine_whole_buffer() {
    let d = host().describe(COMPONENT, "bitswap").expect("describe");
    assert_eq!(d.seek, SeekClass::Affine);
    assert!(d.random_access_inputs.is_empty());
}

#[test]
fn repeated_runs_are_bit_identical() {
    let host = host();
    let input = pattern();
    let first = host
        .run(COMPONENT, "byteswap", &[], std::slice::from_ref(&input))
        .expect("run");
    // Independent stores/instances each run; any engine-level nondeterminism
    // (JIT, allocation, layout) would surface as a byte diff here.
    for _ in 0..4 {
        let again = host
            .run(COMPONENT, "byteswap", &[], std::slice::from_ref(&input))
            .expect("run");
        assert_eq!(first, again, "two runs of the same recipe diverged");
    }
    // And the output is *correct*, not just stable: byteswap is its own
    // inverse, so applying it to the output must return the input.
    let back = host
        .run(COMPONENT, "byteswap", &[], std::slice::from_ref(&first[0]))
        .expect("run");
    assert_eq!(back[0], input);
}

#[test]
fn golden_output_anchor() {
    let out = host()
        .run(COMPONENT, "bitswap", &[], &[pattern()])
        .expect("run");
    assert_eq!(out.len(), 1);
    assert_eq!(
        blake3::hash(&out[0]).to_hex().as_str(),
        BITSWAP_OUTPUT_BLAKE3,
        "cross-architecture determinism anchor broken"
    );
}

#[test]
fn unknown_op_is_a_transform_error() {
    let err = host()
        .run(COMPONENT, "nonsense", &[], &[pattern()])
        .expect_err("unknown op must fail");
    assert!(matches!(err, RuntimeError::Transform(_)), "got {err:?}");
}

#[test]
fn wrong_input_arity_is_a_transform_error() {
    let err = host()
        .run(COMPONENT, "bitswap", &[], &[pattern(), pattern()])
        .expect_err("swap takes exactly one input");
    assert!(matches!(err, RuntimeError::Transform(_)), "got {err:?}");
}

#[test]
fn fuel_exhaustion_traps_deterministically() {
    let host = TransformHost::new(Limits {
        fuel: 10_000, // far too little to swap 64 KiB
        ..Limits::default()
    })
    .expect("host");
    let err = host
        .run(COMPONENT, "byteswap", &[], &[pattern()])
        .expect_err("must trap on fuel exhaustion");
    assert!(matches!(err, RuntimeError::Trap(_)), "got {err:?}");
}

#[test]
fn memory_ceiling_traps_instead_of_ooming() {
    // Enough memory to instantiate, nowhere near enough for an 8 MiB blob to
    // cross the ABI (lift + copy). The guest's failed allocation must become
    // a trap, not host memory growth.
    let host = TransformHost::new(Limits {
        memory: 2 << 20,
        ..Limits::default()
    })
    .expect("host");
    let big = vec![0xAB; 8 << 20];
    let err = host
        .run(COMPONENT, "byteswap", &[], &[big])
        .expect_err("must trap at the memory ceiling");
    assert!(matches!(err, RuntimeError::Trap(_)), "got {err:?}");
}

#[test]
fn components_with_ambient_imports_cannot_instantiate() {
    // A component demanding a WASI instance: the empty linker (D5: no
    // ambient capabilities) must refuse it outright — this is the enforcement
    // test for "the import surface is the sandbox".
    let wasi_wanting =
        wat::parse_str(r#"(component (import "wasi:cli/environment@0.2.6" (instance)))"#)
            .expect("valid wat");
    let err = host()
        .describe(&wasi_wanting, "anything")
        .expect_err("ambient import must be unlinkable");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("import") || msg.contains("describe"),
        "unexpected failure shape: {msg}"
    );
}
