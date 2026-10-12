//! A metrics recorder for tests: every counter increment and gauge value, by series.
//!
//! Install it with `metrics::set_default_local_recorder` on the thread that runs the code under
//! test. Under the default single-threaded `#[tokio::test]` runtime that is every task the test
//! spawns.

#![allow(
    clippy::expect_used,
    reason = "a test helper over a mutex nothing else holds"
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The series recorded so far, as `name{label=value,...}` (labels sorted) to its value: the sum of
/// the increments for a counter, the last value set for a gauge.
#[derive(Clone, Debug, Default)]
pub(crate) struct Recorded(Arc<Mutex<BTreeMap<String, f64>>>);

impl Recorded {
    /// The recorder to install.
    pub(crate) fn recorder(&self) -> Recorder {
        Recorder(self.clone())
    }

    /// The value of one series, if anything was recorded under it.
    pub(crate) fn get(&self, series: &str) -> Option<f64> {
        self.0
            .lock()
            .expect("the test recorder is uncontended")
            .get(series)
            .copied()
    }

    /// Every series name recorded, for a failure message.
    pub(crate) fn series(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("the test recorder is uncontended")
            .keys()
            .cloned()
            .collect()
    }
}

/// See [`Recorded`].
#[derive(Debug)]
pub(crate) struct Recorder(Recorded);

fn series_of(key: &metrics::Key) -> String {
    let mut labels: Vec<String> = key
        .labels()
        .map(|label| format!("{}={}", label.key(), label.value()))
        .collect();
    labels.sort();
    if labels.is_empty() {
        key.name().to_owned()
    } else {
        format!("{}{{{}}}", key.name(), labels.join(","))
    }
}

struct Cell {
    series: String,
    recorded: Recorded,
}

impl metrics::CounterFn for Cell {
    fn increment(&self, value: u64) {
        let mut all = self.recorded.0.lock().expect("uncontended");
        #[expect(clippy::cast_precision_loss, reason = "test counters are tiny")]
        let added = value as f64;
        *all.entry(self.series.clone()).or_insert(0.0) += added;
    }

    fn absolute(&self, value: u64) {
        let mut all = self.recorded.0.lock().expect("uncontended");
        #[expect(clippy::cast_precision_loss, reason = "test counters are tiny")]
        let set = value as f64;
        all.insert(self.series.clone(), set);
    }
}

impl metrics::GaugeFn for Cell {
    fn increment(&self, value: f64) {
        let mut all = self.recorded.0.lock().expect("uncontended");
        *all.entry(self.series.clone()).or_insert(0.0) += value;
    }

    fn decrement(&self, value: f64) {
        let mut all = self.recorded.0.lock().expect("uncontended");
        *all.entry(self.series.clone()).or_insert(0.0) -= value;
    }

    fn set(&self, value: f64) {
        self.recorded
            .0
            .lock()
            .expect("uncontended")
            .insert(self.series.clone(), value);
    }
}

impl metrics::Recorder for Recorder {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }

    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        metrics::Counter::from_arc(Arc::new(Cell {
            series: series_of(key),
            recorded: self.0.clone(),
        }))
    }

    fn register_gauge(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        metrics::Gauge::from_arc(Arc::new(Cell {
            series: series_of(key),
            recorded: self.0.clone(),
        }))
    }

    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}
