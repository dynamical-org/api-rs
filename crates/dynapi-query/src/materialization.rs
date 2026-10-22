use crate::*;

pub const DEFAULT_MATERIALIZATION_BYTES: u128 = 64 * 1024 * 1024;

/// Cumulative conservative charge, reserved before selection-owned allocation.
/// It covers retained selections and subsequent presentation/encoding staging;
/// opened source metadata and caller-owned validated input remain separate.
pub struct MaterializationBudget {
    limit: u128,
    used: u128,
}
impl Default for MaterializationBudget {
    fn default() -> Self {
        Self::new(DEFAULT_MATERIALIZATION_BYTES)
    }
}
impl MaterializationBudget {
    pub fn new(limit: u128) -> Self {
        Self { limit, used: 0 }
    }
    pub(crate) fn reserve(&mut self, bytes: Option<u128>) -> Result<(), QueryError> {
        let total = bytes.and_then(|n| self.used.checked_add(n));
        if total.is_none_or(|n| n > self.limit) {
            return Err(QueryError::invalid(format!(
                "Query exceeds the {}-byte response materialization budget",
                self.limit
            )));
        }
        self.used = total.unwrap();
        Ok(())
    }
    pub(crate) fn copy_labels(
        &mut self,
        labels: &[String],
        id: Option<&str>,
    ) -> Result<(), QueryError> {
        for label in labels.iter().map(String::as_str).chain(id) {
            self.reserve(text(label))?;
        }
        Ok(())
    }
    pub(crate) fn query(
        &mut self,
        view: &DatasetView,
        collection: &Collection,
        variables: &[String],
        snapshot: &str,
    ) -> Result<(), QueryError> {
        self.reserve(base_bytes(view, collection, variables, snapshot))
    }
    pub(crate) fn time(&mut self) -> Result<(), QueryError> {
        // A timestamp is at most 27 bytes. This covers its 24x presentation
        // charge plus selectors, String slots, lead hours and Vec growth slack.
        self.reserve(Some(1536))
    }
    pub(crate) fn values(&mut self, count: u64) -> Result<(), QueryError> {
        self.reserve((count as u128).checked_mul(512))
    }
}
pub(crate) fn text(s: &str) -> Option<u128> {
    (s.len() as u128).checked_mul(24)?.checked_add(256)
}
fn value(v: &Value) -> Option<u128> {
    (dynapi_contract::output::json_byte_bound(v)? as u128)
        .checked_mul(8)?
        .checked_add(256)
}
pub(crate) fn base_bytes(
    view: &DatasetView,
    collection: &Collection,
    variables: &[String],
    snapshot: &str,
) -> Option<u128> {
    let mut bytes = 4096u128;
    for s in [collection.id.as_str(), snapshot]
        .into_iter()
        .chain(collection.version.as_deref())
        .chain(collection.catalog_url.as_deref())
    {
        bytes = bytes.checked_add(text(s)?)?;
    }
    for name in variables {
        bytes = bytes.checked_add(text(name)?)?;
        if let Some(unit) = collection.variables.get(name).and_then(|v| v.unit.as_ref()) {
            bytes = bytes.checked_add(text(unit)?)?;
        } else if let Some(unit) = view
            .variables
            .get(name)
            .and_then(|v| v.attributes.get("units"))
        {
            bytes = bytes.checked_add(value(unit)?)?;
        }
    }
    if variables.iter().any(|n| {
        view.variables
            .get(n)
            .is_some_and(|v| v.dimensions.iter().any(|d| d == "ensemble_member"))
    }) {
        if let Some(members) = view.dimensions.get("ensemble_member") {
            bytes = bytes.checked_add(
                (*members as u128).checked_mul(512 + variables.len() as u128 * 256)?,
            )?;
        }
        if let Some(Coordinate::Members(ids)) = view.coordinates.get("ensemble_member") {
            for id in ids {
                bytes = bytes.checked_add(value(id)?)?;
            }
        }
    }
    Some(bytes)
}
