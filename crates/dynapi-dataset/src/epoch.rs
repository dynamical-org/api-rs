//! Bounded selector staging for large integer-epoch dimension coordinates.
use crate::OpenError;
use dynapi_query::Coordinate;
use dynapi_reader::{ErrorKind, ReaderError, Selector, TypedBuffer, cf_time};
use serde_json::Value;
use std::{future::Future, time::Duration};

const BATCH: usize = 262_144;

pub(crate) fn eligible(meta: &Value, cf: Option<&cf_time::Request>) -> bool {
    // Same exact epoch tuple admitted by the reader. Unlike float durations,
    // integer epochs have no value-dependent full-axis conversion context.
    meta["data_type"] == "int64"
        && cf.is_some_and(|cf| {
            cf.revision == cf_time::REVISION
                && cf.attributes.len() == 2
                && cf.attributes.get("units").and_then(Value::as_str)
                    == Some("seconds since 1970-01-01")
                && cf.attributes.get("calendar").and_then(Value::as_str)
                    == Some("proleptic_gregorian")
        })
}

pub(crate) fn charge(size: usize, groups: usize, name_len: usize) -> Option<usize> {
    // Retained exact-sized i64 axis, one batch's existing conservative staging,
    // and all groups (an upper bound on any one batch), plus the coordinate key.
    size.checked_mul(8)?
        .checked_add(size.min(BATCH).checked_mul(160)?)?
        .checked_add(groups.checked_mul(512)?)?
        .checked_add(name_len.checked_add(64)?)
}

pub(crate) async fn read<F, Fut>(size: usize, mut read_batch: F) -> Result<Coordinate, OpenError>
where
    F: FnMut(Vec<Selector>) -> Fut,
    Fut: Future<Output = Result<TypedBuffer, ReaderError>>,
{
    // One full-axis deadline, not a fresh deadline for each selection. Dropping
    // this future drops the retained axis, selectors and in-flight batch together.
    tokio::time::timeout(Duration::from_secs(90), async {
        let mut values = Vec::with_capacity(size);
        for start in (0..size).step_by(BATCH) {
            let end = (start + BATCH).min(size);
            let selectors = vec![Selector::Indices((start..end).map(|i| i as i64).collect())];
            let buffer = read_batch(selectors).await?;
            if buffer.dtype != "datetime64[ns]"
                || buffer.shape != [end - start]
                || buffer.bytes.len() != (end - start) * 8
            {
                return Err(OpenError::Metadata(
                    "Invalid decoded epoch coordinate".into(),
                ));
            }
            values.extend(
                buffer
                    .bytes
                    .chunks_exact(8)
                    .map(|bytes| i64::from_le_bytes(bytes.try_into().expect("eight-byte epoch"))),
            );
        }
        Ok(Coordinate::Time(values))
    })
    .await
    .map_err(|_| {
        OpenError::Reader(ReaderError {
            kind: ErrorKind::Storage,
            message: "Reader timed out after 90 seconds".into(),
            virtual_chunk_fetch: false,
            exception_class: None,
        })
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };

    #[test]
    fn epoch_plan_requires_the_exact_encoding_and_checked_charge() {
        let meta = json!({"data_type":"int64"});
        let mut cf = cf_time::Request {
            revision: cf_time::REVISION.into(),
            attributes:
                json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"})
                    .as_object()
                    .unwrap()
                    .clone(),
        };
        assert!(eligible(&meta, Some(&cf)));
        assert!(!eligible(&json!({"data_type":"float64"}), Some(&cf)));
        assert!(!eligible(&meta, None));
        for attrs in [
            json!({}),
            json!({"units":"seconds since 1970-01-01","calendar":"standard"}),
            json!({"dtype":"datetime64[ns]","calendar":"proleptic_gregorian"}),
            json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian","_FillValue":0}),
        ] {
            cf.attributes = attrs.as_object().unwrap().clone();
            assert!(!eligible(&meta, Some(&cf)));
        }
        assert_eq!(
            charge(BATCH + 1, 2, 4),
            Some((BATCH + 1) * 8 + BATCH * 160 + 2 * 512 + 68)
        );
        let cost = charge(BATCH + 1, 2, 4).unwrap();
        let mut budget = crate::metadata::Budget::new(crate::MAX_METADATA_BYTES);
        budget.add(crate::MAX_METADATA_BYTES - cost).unwrap();
        let used = budget.used();
        assert!(budget.fits(cost));
        assert!(!budget.fits(cost + 1));
        assert_eq!(budget.used(), used, "planning must not charge");
        budget.add(cost).unwrap();
        assert_eq!(budget.used(), crate::MAX_METADATA_BYTES);
        assert_eq!(charge(usize::MAX, 1, 4), None);
        assert_eq!(charge(1, usize::MAX, 4), None);
        assert_eq!(charge(1, 1, usize::MAX), None);
    }

    #[tokio::test]
    async fn epoch_batches_preserve_order_and_exact_boundaries() {
        for size in [BATCH - 1, BATCH, BATCH + 1, 2 * BATCH, 2 * BATCH + 1] {
            let calls = AtomicUsize::new(0);
            let result = read(size, |selectors| {
                let position = calls.fetch_add(1, SeqCst);
                async move {
                    let [Selector::Indices(indices)] = selectors.as_slice() else {
                        panic!("indices");
                    };
                    assert_eq!(indices[0] as usize, position * BATCH);
                    assert_eq!(indices.len(), (size - position * BATCH).min(BATCH));
                    Ok(TypedBuffer {
                        dtype: "datetime64[ns]".into(),
                        shape: vec![indices.len()],
                        bytes: indices.iter().flat_map(|i| i.to_le_bytes()).collect(),
                    })
                }
            })
            .await
            .unwrap();
            let Coordinate::Time(values) = result else {
                panic!("time");
            };
            assert_eq!(values.capacity(), size);
            assert!(values.iter().enumerate().all(|(i, x)| *x == i as i64));
            assert_eq!(calls.load(SeqCst), size.div_ceil(BATCH));
        }
    }

    struct Active(Arc<AtomicUsize>);
    impl Active {
        fn new(count: &Arc<AtomicUsize>) -> Self {
            count.fetch_add(1, SeqCst);
            Self(count.clone())
        }
    }
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn epoch_deadline_spans_all_batches_and_drops_inflight_work() {
        let active = Arc::new(AtomicUsize::new(0));
        let calls = AtomicUsize::new(0);
        let start = tokio::time::Instant::now();
        let result = read(BATCH + 1, |selectors| {
            let call = calls.fetch_add(1, SeqCst);
            let guard = Active::new(&active);
            async move {
                let _guard = guard;
                tokio::time::sleep(Duration::from_secs(if call == 0 { 89 } else { 2 })).await;
                let [Selector::Indices(indices)] = selectors.as_slice() else {
                    panic!("indices");
                };
                Ok(TypedBuffer {
                    dtype: "datetime64[ns]".into(),
                    shape: vec![indices.len()],
                    bytes: vec![0; indices.len() * 8],
                })
            }
        })
        .await;
        assert!(
            matches!(result, Err(OpenError::Reader(e)) if e.kind == ErrorKind::Storage && e.message == "Reader timed out after 90 seconds")
        );
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(90));
        assert_eq!(calls.load(SeqCst), 2);
        assert_eq!(active.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn cancelling_epoch_axis_drops_inflight_selectors() {
        let active = Arc::new(AtomicUsize::new(0));
        let notified = Arc::new(tokio::sync::Notify::new());
        let (notify, count) = (notified.clone(), active.clone());
        let caller = tokio::spawn(async move {
            let calls = AtomicUsize::new(0);
            read(BATCH + 1, |selectors| {
                let call = calls.fetch_add(1, SeqCst);
                let guard = Active::new(&count);
                let notify = notify.clone();
                async move {
                    let _guard = guard;
                    if call == 1 {
                        notify.notify_one();
                        std::future::pending::<()>().await;
                    }
                    let [Selector::Indices(indices)] = selectors.as_slice() else {
                        panic!("indices");
                    };
                    Ok(TypedBuffer {
                        dtype: "datetime64[ns]".into(),
                        shape: vec![indices.len()],
                        bytes: vec![0; indices.len() * 8],
                    })
                }
            })
            .await
        });
        notified.notified().await;
        assert_eq!(active.load(SeqCst), 1);
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(active.load(SeqCst), 0);
    }
}
