use crate::selection::{checked_sum, chunk};
use crate::*;

/// Source identity deliberately distinguishes separately opened dataset views,
/// even when their product and snapshot strings match (Python uses id(source)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoTileKey {
    pub source_identity: usize,
    pub collection_id: String,
    pub snapshot_id: String,
    pub variables: Vec<String>,
    pub non_spatial: Vec<(String, Selection)>,
    pub tiles: Vec<(String, String, u64)>,
}
pub fn co_tile_key(query: &PreparedQuery<'_>) -> Option<CoTileKey> {
    let spatial = &query.point.indexers;
    let mut tiles = Vec::new();
    for name in &query.options.variables {
        let var = &query.source.variables[name];
        for (axis, dim) in var.dimensions.iter().enumerate() {
            let Some((_, index)) = spatial.iter().find(|(d, _)| d == dim) else {
                continue;
            };
            let chunk = chunk(var, axis, dim)?;
            if chunk <= 0 {
                return None;
            }
            tiles.push((name.clone(), dim.clone(), *index as u64 / chunk as u64));
        }
    }
    Some(CoTileKey {
        source_identity: query.source as *const DatasetView as usize,
        collection_id: query.collection.id.clone(),
        snapshot_id: query.snapshot_id.clone(),
        variables: query.options.variables.clone(),
        non_spatial: query
            .positions
            .iter()
            .filter(|(d, _)| !spatial.iter().any(|(s, _)| s == *d))
            .map(|(d, s)| (d.clone(), s.clone()))
            .collect(),
        tiles,
    })
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadPlan {
    /// Selection indices, in first-group-occurrence order. Execution must restore
    /// request order, because groups may contain nonadjacent selection indices.
    pub groups: Vec<Vec<usize>>,
    pub selections: usize,
    pub ungroupable_selections: usize,
    pub co_tiled_queries: usize,
    pub grouped_read_values: u64,
}
pub fn plan_load_groups(
    prepared: &[PreparedQuery<'_>],
    grouped: bool,
    max_group_points: usize,
) -> Result<LoadPlan, QueryError> {
    let mut groups: Vec<(Option<CoTileKey>, Vec<usize>)> = Vec::new();
    let mut ungroupable = 0;
    for (index, query) in prepared.iter().enumerate() {
        let key = co_tile_key(query);
        if key.is_none() {
            ungroupable += 1;
        }
        if let Some((_, members)) = groups.iter_mut().find(|(k, _)| key.is_some() && *k == key) {
            members.push(index);
        } else {
            groups.push((key, vec![index]));
        }
    }
    let co_tiled_queries = groups
        .iter()
        .filter(|(_, members)| members.len() > 1)
        .map(|(_, members)| members.len())
        .sum();
    let groups = if grouped {
        groups
            .into_iter()
            .flat_map(|(_, members)| {
                members
                    .chunks(max_group_points.max(1))
                    .map(<[usize]>::to_vec)
                    .collect::<Vec<_>>()
            })
            .collect()
    } else {
        (0..prepared.len()).map(|i| vec![i]).collect::<Vec<_>>()
    };
    let grouped_read_values = checked_sum(groups.iter().map(|g| prepared[g[0]].read_values))?;
    Ok(LoadPlan {
        groups,
        selections: prepared.len(),
        ungroupable_selections: ungroupable,
        co_tiled_queries,
        grouped_read_values,
    })
}
