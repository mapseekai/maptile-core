use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use composite_mvt::{Compression, MvtComposer, MvtSource};
use tokio::sync::Semaphore;

use super::{CompiledComposite, CompositeDeclaration, CompositeLimits, DuplicateLayer, Member};
use crate::{
    cache::{CacheZoomRange, ContentVersion},
    control::{SourceCapabilities, SourceDescriptor, catalog::validate_id, lifecycle::SourceSlot},
    error::{MaptileError, MaptileResult},
    sources::{SourceKind, TileEncoding, TileFormat, TileInfo},
    transport,
};

pub(in crate::control) fn compile_graph(
    sources: &HashMap<Arc<str>, Arc<SourceSlot>>,
    declarations: &[CompositeDeclaration],
    limits: CompositeLimits,
) -> MaptileResult<HashMap<Arc<str>, Arc<CompiledComposite>>> {
    if limits.max_depth == 0
        || limits.max_depth > 64
        || limits.max_leaves == 0
        || limits.max_concurrency == 0
        || limits.max_concurrency > Semaphore::MAX_PERMITS
        || limits.max_decoded_bytes == 0
        || limits.max_compressed_bytes == 0
        || limits.max_work_bytes == 0
    {
        return Err(MaptileError::Config("invalid composite limits".into()));
    }
    let mut defs = HashMap::new();
    for decl in declarations {
        for member in &decl.members {
            validate_id(member)?;
        }
        if sources.contains_key(&decl.id) || defs.insert(decl.id.clone(), decl).is_some() {
            return Err(MaptileError::Config(format!(
                "duplicate or empty composite id {}",
                decl.id
            )));
        }
        if decl.members.is_empty() || decl.duplicate_layer == DuplicateLayer::Allow {
            return Err(MaptileError::Config(format!(
                "invalid members or duplicate policy for {}",
                decl.id
            )));
        }
    }
    fn check(
        id: &Arc<str>,
        defs: &HashMap<Arc<str>, &CompositeDeclaration>,
        active: &mut HashSet<Arc<str>>,
        done: &mut HashMap<Arc<str>, usize>,
        depth_now: usize,
        limit: usize,
    ) -> MaptileResult<usize> {
        if depth_now > limit {
            return Err(MaptileError::Config("composite graph too deep".into()));
        }
        if active.contains(id) {
            return Err(MaptileError::Config(format!("composite cycle at {id}")));
        }
        if let Some(depth) = done.get(id) {
            if depth_now + depth - 1 > limit {
                return Err(MaptileError::Config("composite graph too deep".into()));
            }
            return Ok(*depth);
        }
        if let Some(decl) = defs.get(id) {
            active.insert(id.clone());
            let mut depth = 1;
            for child in &decl.members {
                if defs.contains_key(child) {
                    depth = depth.max(1 + check(child, defs, active, done, depth_now + 1, limit)?);
                }
            }
            active.remove(id);
            if depth > limit {
                return Err(MaptileError::Config("composite graph too deep".into()));
            }
            done.insert(id.clone(), depth);
            return Ok(depth);
        }
        Ok(0)
    }
    let mut done = HashMap::new();
    for id in defs.keys() {
        check(
            id,
            &defs,
            &mut HashSet::new(),
            &mut done,
            1,
            limits.max_depth,
        )?;
    }
    let mut built = HashMap::new();
    for id in defs.keys() {
        build_node(id, sources, &defs, &mut built, limits)?;
    }
    Ok(built)
}

fn build_node(
    id: &Arc<str>,
    sources: &HashMap<Arc<str>, Arc<SourceSlot>>,
    defs: &HashMap<Arc<str>, &CompositeDeclaration>,
    built: &mut HashMap<Arc<str>, Arc<CompiledComposite>>,
    limits: CompositeLimits,
) -> MaptileResult<Arc<CompiledComposite>> {
    if let Some(node) = built.get(id) {
        return Ok(node.clone());
    }
    let decl = defs[id];
    let mut members = Vec::new();
    let mut leaves = Vec::new();
    let mut leaf_ids = HashSet::new();
    for member_id in &decl.members {
        let member = if let Some(slot) = sources.get(member_id) {
            Some(Member::Source(slot.clone()))
        } else if defs.contains_key(member_id) {
            let node = build_node(member_id, sources, defs, built, limits)?;
            (node.composer.is_some()).then_some(Member::Composite(node))
        } else {
            None
        };
        let Some(member) = member else {
            if decl.lazy {
                continue;
            }
            return Err(MaptileError::Config(format!(
                "missing composite member {member_id} in {id}"
            )));
        };
        let descriptor = member.descriptor();
        if descriptor.info.format != TileFormat::Mvt
            || !descriptor.capabilities.strict_mvt
            || descriptor
                .tilejson
                .vector_layers
                .as_ref()
                .is_none_or(Vec::is_empty)
        {
            return Err(MaptileError::Config(format!(
                "member {member_id} lacks strict MVT declarations"
            )));
        }
        let children: Vec<_> = match &member {
            Member::Source(slot) => vec![slot.clone()],
            Member::Composite(node) => node.leaves.to_vec(),
        };
        for child in children {
            if !leaf_ids.insert(child.descriptor().id.clone()) {
                return Err(MaptileError::Config(format!(
                    "duplicate leaf {} in {id}",
                    child.descriptor().id
                )));
            }
            leaves.push(child);
            if leaves.len() > limits.max_leaves {
                return Err(MaptileError::Config(format!("too many leaves in {id}")));
            }
        }
        members.push(member);
    }
    let mut builder = MvtComposer::builder()
        .duplicate_layer(decl.duplicate_layer)
        .output_compression(Compression::None)
        .max_decoded_bytes(
            limits
                .max_decoded_bytes
                .min(transport::MAX_DECODED_TILE_BYTES),
        );
    let mut layers = Vec::<tilejson::VectorLayer>::new();
    let mut tj = tilejson::tilejson! { tiles: vec![] };
    tj.name = Some(format!("composite:{id}"));
    let mut caps = SourceCapabilities {
        filter: true,
        bbox: true,
        url_query: true,
        raster_style: !members.is_empty(),
        strict_mvt: true,
        consistent_export: true,
        ..Default::default()
    };
    let mut zoom = CacheZoomRange::default();
    let mut hash = blake3::Hasher::new();
    hash.update(
        format!(
            "composite-v1:{:?}:{:?}",
            decl.duplicate_layer, decl.failure_policy
        )
        .as_bytes(),
    );
    for (index, member) in members.iter().enumerate() {
        let d = member.descriptor();
        let own_layers = d.tilejson.vector_layers.as_ref().expect("checked above");
        builder = builder.add_source(
            MvtSource::new(d.id.to_string()).with_layers(own_layers.iter().map(|l| l.id.clone())),
        );
        for layer in own_layers {
            if let Some(existing) = layers.iter_mut().find(|existing| existing.id == layer.id) {
                for (field, description) in &layer.fields {
                    existing
                        .fields
                        .entry(field.clone())
                        .and_modify(|current| {
                            if current != description {
                                *current = "Varies by source".into();
                            }
                        })
                        .or_insert_with(|| description.clone());
                }
                existing.minzoom = existing.minzoom.zip(layer.minzoom).map(|(a, b)| a.min(b));
                existing.maxzoom = existing.maxzoom.zip(layer.maxzoom).map(|(a, b)| a.max(b));
            } else {
                layers.push(layer.clone());
            }
        }
        tj.bounds = if index == 0 {
            d.tilejson.bounds
        } else {
            tj.bounds.zip(d.tilejson.bounds).map(|(a, b)| {
                tilejson::Bounds::new(
                    a.left.min(b.left),
                    a.bottom.min(b.bottom),
                    a.right.max(b.right),
                    a.top.max(b.top),
                )
            })
        };
        tj.minzoom = match (tj.minzoom, d.tilejson.minzoom) {
            (None, z) if index == 0 => z,
            (Some(a), Some(b)) => Some(a.min(b)),
            _ => None,
        };
        tj.maxzoom = match (tj.maxzoom, d.tilejson.maxzoom) {
            (None, z) if index == 0 => z,
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        caps.filter &= d.capabilities.filter;
        caps.bbox &= d.capabilities.bbox;
        caps.url_query &= d.capabilities.url_query;
        caps.raster_style &= d.capabilities.raster_style;
        caps.consistent_export &= d.capabilities.consistent_export;
        zoom = zoom.intersection(d.cache_zoom);
        hash.update(&(d.id.len() as u64).to_le_bytes());
        hash.update(d.id.as_bytes());
        hash.update(d.revision.as_bytes());
    }
    if !layers.is_empty() {
        tj.vector_layers = Some(layers);
    }
    let composer = if members.is_empty() {
        None
    } else {
        Some(Arc::new(builder.build().map_err(|e| {
            MaptileError::Config(format!("composite {id}: {e}"))
        })?))
    };
    let descriptor = SourceDescriptor {
        id: id.clone(),
        kind: SourceKind::Composite,
        tilejson: tj,
        info: TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        },
        revision: ContentVersion::from_bytes(*hash.finalize().as_bytes()),
        cache_zoom: zoom,
        capabilities: caps,
    };
    let node = Arc::new(CompiledComposite {
        descriptor,
        leaves: leaves.into_boxed_slice(),
        members: members.into_boxed_slice(),
        composer,
        policy: decl.failure_policy,
    });
    built.insert(id.clone(), node.clone());
    Ok(node)
}
