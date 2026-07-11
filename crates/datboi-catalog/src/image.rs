//! Reified images (D62): a view snapshot rendered as ONE `assemble@1`
//! recipe whose output is a complete FAT32 disk image — skeleton blobs
//! (FAT, directory clusters) + inline literal sectors + cluster-aligned
//! windows over content blobs + zero fill. Policies emit recipes (D23);
//! this is the policy tier's layout math ([`crate::fat32`]) turned into
//! a claim the executor can serve and verify like any other route.
//!
//! Identity: same snapshot ⇒ bit-identical recipe and image (volume
//! serial and disk signature derive from the snapshot hash; timestamps
//! are fixed). The mint streams the assembled output once to compute
//! the claimed output hash; the obao sidecar falls out of the same pass
//! (D49-blessed from birth) unless the caller opts out — the D63
//! carve-out then covers serving.

use std::fs::File;

use datboi_core::assemble::{self, AssembleParams, Segment, Source};
use datboi_core::hash::Blake3;
use datboi_core::recipe::{InputRef, Op, OutputRef, Recipe};
use datboi_core::viewsnap::ViewSnapshot;
use datboi_index::{
    Db, Namespace as IndexNs, OpKind, RecipeSource, Residency, SeekClass, VerifyState,
    recipes::NewRecipe,
};
use datboi_store_fs::{Namespace as StoreNs, Store, obao};
use positioned_io::ReadAt;

use crate::CatalogError;
use crate::fat32::{self, Fat32Params, FileEntry, LayoutSegment, label_for};

/// User-facing image parameters (the ViewDef additions; defaults match
/// the SD-card conventions ruled 2026-07-10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageParams {
    /// Bytes per cluster (power of two, 512..=65536).
    pub cluster_size: u32,
    /// MBR partition table (default) vs superfloppy.
    pub partition: bool,
    /// Volume label; defaults to the view name ([`label_for`]).
    pub label: Option<String>,
}

impl Default for ImageParams {
    fn default() -> Self {
        Self {
            cluster_size: 32 * 1024,
            partition: true,
            label: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReport {
    /// Output blob hash — what `image/<view>` points at.
    pub image: Blake3,
    /// The recipe object's hash.
    pub recipe: Blake3,
    /// Image size in bytes.
    pub size: u64,
    /// Bytes of newly minted skeleton (FAT + directory clusters).
    pub skeleton_bytes: u64,
    /// Whether the output obao sidecar was stored (full-D49 serving).
    pub obao_stored: bool,
    /// Manifest rows laid out.
    pub rows: usize,
}

/// Content hashes the mint needs resident but which aren't. The CLI
/// materializes these before calling [`mint_image`].
pub fn missing_inputs(db: &Db, snap: &ViewSnapshot) -> Result<Vec<Blake3>, CatalogError> {
    let mut missing = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in &snap.rows {
        if !seen.insert(row.hash) {
            continue;
        }
        let resident = db
            .blob_by_hash(&row.hash)?
            .is_some_and(|b| b.residency == Residency::Resident);
        if !resident {
            missing.push(row.hash);
        }
    }
    Ok(missing)
}

/// An assemble input source: in-memory skeleton or a store file.
enum Src {
    Mem(Vec<u8>),
    File { file: File, len: u64 },
}

impl Source for Src {
    fn len(&self) -> u64 {
        match self {
            Src::Mem(b) => b.len() as u64,
            Src::File { len, .. } => *len,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        match self {
            Src::Mem(b) => Source::read_at(&b.as_slice(), offset, buf),
            Src::File { file, .. } => {
                let mut filled = 0usize;
                while filled < buf.len() {
                    match ReadAt::read_at(file, offset + filled as u64, &mut buf[filled..]) {
                        Ok(0) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "input blob shorter than indexed size",
                            ));
                        }
                        Ok(n) => filled += n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
        }
    }
}

/// Mint the FAT32 image recipe for `snap` and move the `image/<name>`
/// tag to its output (D33 flip + D27 GC root in one move).
///
/// Deterministic: re-minting the same snapshot with the same params is
/// a content-addressed no-op. Every content row must be resident
/// (check [`missing_inputs`] first). With `store_obao` the output
/// sidecar is published in the same streaming pass that computes the
/// output hash — the image serves full-D49 from birth; without it,
/// serving relies on the D63 affine carve-out.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn mint_image(
    db: &mut Db,
    store: &Store,
    view_name: &str,
    snap_hash: &Blake3,
    snap: &ViewSnapshot,
    params: &ImageParams,
    store_obao: bool,
    now: i64,
) -> Result<ImageReport, CatalogError> {
    // Layout (pure math; refusals surface as Fat32 errors).
    let files: Vec<FileEntry> = snap
        .rows
        .iter()
        .map(|r| FileEntry {
            path: r.path.clone(),
            size: r.size,
        })
        .collect();
    let fp = Fat32Params {
        volume_label: params
            .label
            .as_deref()
            .map_or_else(|| label_for(view_name), label_for),
        serial: u32::from_le_bytes(snap_hash.0[0..4].try_into().expect("4 bytes")),
        disk_signature: u32::from_le_bytes(snap_hash.0[4..8].try_into().expect("4 bytes")),
        cluster_size: params.cluster_size,
        partition: params.partition,
    };
    let layout = fat32::layout(&files, &fp)?;

    // Skeleton blobs: hashed by us, resident, verified (D4).
    let fat_hash = Blake3::compute(&layout.fat);
    let dirs_hash = Blake3::compute(&layout.dirs);
    store.put(StoreNs::Data, fat_hash, layout.fat.as_slice())?;
    store.put(StoreNs::Data, dirs_hash, layout.dirs.as_slice())?;
    for (hash, len) in [
        (&fat_hash, layout.fat.len() as u64),
        (&dirs_hash, layout.dirs.len() as u64),
    ] {
        let id = db.upsert_blob(hash, Some(len), IndexNs::Data, Residency::Resident)?;
        db.set_verified(id, now)?;
    }
    let skeleton_bytes = (layout.fat.len() + layout.dirs.len()) as u64;

    // Input table: 0 = FAT, 1 = dirs, then unique content hashes in
    // first-appearance (row) order. A blob at two paths is one input
    // with two windows.
    let mut input_hashes: Vec<Blake3> = vec![fat_hash, dirs_hash];
    let mut input_ix_of = std::collections::HashMap::new();
    for row in &snap.rows {
        input_ix_of.entry(row.hash).or_insert_with(|| {
            input_hashes.push(row.hash);
            u32::try_from(input_hashes.len() - 1).expect("input count fits u32")
        });
    }

    // Open + validate content inputs (resident, size matches manifest).
    let mut sources: Vec<Src> = vec![Src::Mem(layout.fat.clone()), Src::Mem(layout.dirs.clone())];
    let mut content_blob_ids: Vec<i64> = Vec::new();
    let mut missing = 0usize;
    for hash in &input_hashes[2..] {
        let Some(blob) = db.blob_by_hash(hash)? else {
            missing += 1;
            continue;
        };
        let Some(file) = store.get(StoreNs::Data, hash)? else {
            missing += 1;
            continue;
        };
        let len = file.metadata()?.len();
        if blob.size.is_some_and(|s| s != len) {
            return Err(CatalogError::Image(format!(
                "input {hash} is {len} bytes on disk but indexed as {:?}",
                blob.size
            )));
        }
        content_blob_ids.push(blob.blob_id);
        sources.push(Src::File { file, len });
    }
    if missing > 0 {
        return Err(CatalogError::Image(format!(
            "{missing} content inputs not resident — materialize them first \
             (see `missing_inputs`)"
        )));
    }

    // Symbolic layout segments → assemble segments.
    let segments: Vec<Segment> = layout
        .segments
        .iter()
        .map(|s| match s {
            LayoutSegment::Literal(bytes) => Segment::Literal {
                bytes: bytes.clone(),
            },
            LayoutSegment::Fat { offset, len } => Segment::BlobRange {
                input_ix: 0,
                offset: *offset,
                len: *len,
            },
            LayoutSegment::Dirs { offset, len } => Segment::BlobRange {
                input_ix: 1,
                offset: *offset,
                len: *len,
            },
            LayoutSegment::File { file_ix, len } => Segment::BlobRange {
                input_ix: input_ix_of[&snap.rows[*file_ix].hash],
                offset: 0,
                len: *len,
            },
            LayoutSegment::Fill { len } => Segment::Fill { byte: 0, len: *len },
        })
        .collect();
    let assemble_params = AssembleParams { segments };
    let total_size = layout.geometry.total_size;
    debug_assert_eq!(
        assemble_params.output_size().ok(),
        Some(total_size),
        "layout and assemble sizes agree"
    );
    let params_bytes = assemble_params
        .encode()
        .map_err(|e| CatalogError::Image(format!("assemble params: {e}")))?;

    // One streaming pass over the assembled output: the claimed output
    // hash and the obao sidecar together (exactly `put_with_obao` minus
    // the tee-to-disk — the image itself is never materialized here).
    let reader = assemble::reader(&assemble_params, &sources)
        .map_err(|e| CatalogError::Image(format!("assemble validation: {e}")))?;
    let (image_hash, sidecar) = obao::compute(reader, total_size)
        .map_err(|e| CatalogError::Image(format!("output hash pass: {e}")))?;

    let image_blob_id = db.upsert_blob(
        &image_hash,
        Some(total_size),
        IndexNs::Data,
        Residency::Absent,
    )?;
    let obao_stored = if store_obao {
        store.put_obao(StoreNs::Data, &image_hash, &sidecar)?;
        true
    } else {
        false
    };

    // The recipe object + row: a private twin of ingest's `mint_recipe`
    // (crates/datboi-ingest/src/lib.rs) — the policy crate must not
    // drag the ingest analyzers in for these ~30 lines. Idempotent by
    // content address, like the original.
    let recipe = Recipe {
        op: Op::Builtin {
            name: "assemble".into(),
            major: 1,
        },
        inputs: input_hashes
            .iter()
            .enumerate()
            .map(|(ix, hash)| InputRef {
                hash: *hash,
                role: match ix {
                    0 => Some("fat".into()),
                    1 => Some("dirs".into()),
                    _ => None,
                },
            })
            .collect(),
        outputs: vec![OutputRef {
            hash: image_hash,
            size: total_size,
            name: None,
        }],
        params: params_bytes,
    };
    let encoded = recipe
        .encode()
        .map_err(|e| CatalogError::Image(format!("recipe encode: {e}")))?;
    let recipe_hash = Blake3::compute(&encoded);
    store.put(StoreNs::Meta, recipe_hash, encoded.as_slice())?;
    let recipe_blob_id = db.upsert_blob(
        &recipe_hash,
        Some(encoded.len() as u64),
        IndexNs::Meta,
        Residency::Resident,
    )?;
    let already: Option<i64> = {
        let mut stmt = db
            .cache()
            .prepare_cached("SELECT recipe_id FROM recipe WHERE blob_id = ?1")?;
        let mut rows = stmt.query((recipe_blob_id,))?;
        rows.next()?.map(|row| row.get(0)).transpose()?
    };
    if already.is_none() {
        let fat_blob_id = db
            .blob_by_hash(&fat_hash)?
            .expect("skeleton upserted above")
            .blob_id;
        let dirs_blob_id = db
            .blob_by_hash(&dirs_hash)?
            .expect("skeleton upserted above")
            .blob_id;
        let mut inputs: Vec<(u32, i64, Option<&str>)> = vec![
            (0, fat_blob_id, Some("fat")),
            (1, dirs_blob_id, Some("dirs")),
        ];
        for (i, blob_id) in content_blob_ids.iter().enumerate() {
            inputs.push((
                u32::try_from(i + 2).expect("input count fits u32"),
                *blob_id,
                None,
            ));
        }
        let recipe_id = db.insert_recipe(&NewRecipe {
            blob_id: recipe_blob_id,
            op_kind: OpKind::Builtin,
            op_name: "assemble@1",
            seek_class: SeekClass::Affine,
            source: RecipeSource::LocalIngest,
            inputs: &inputs,
            outputs: &[(0, image_blob_id, total_size, None)],
        })?;
        db.set_verify_state(recipe_id, VerifyState::Verified, now, None)?;
    }

    // The flip: `image/<name>` is the pin and the serving root.
    db.set_tag(&format!("image/{view_name}"), &image_hash, now)?;

    Ok(ImageReport {
        image: image_hash,
        recipe: recipe_hash,
        size: total_size,
        skeleton_bytes,
        obao_stored,
        rows: snap.rows.len(),
    })
}
