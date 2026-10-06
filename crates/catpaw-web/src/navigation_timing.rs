//! Navigation timing: when the document's loading reached its milestones,
//! as the `navigation` performance entry and the legacy
//! `performance.timing` / `performance.navigation`.
//!
//! The page knows when it parsed and fired its events; what the network
//! did before the document arrived is told to it by the embedder through
//! [`PageState::set_fetch_timing`], and reads as zero otherwise. No other
//! resource is timed.

use std::cell::Cell;

use catpaw_js::{Fallible, ObjectId, Value};

use crate::generated::{self as web, NavigationTimingType};
use crate::page::{Cx, PageState};
use crate::{Web, platform_object};

/// Milliseconds since the time origin for each milestone; 0 until reached.
#[derive(Default)]
pub struct DocumentTiming {
    pub fetch_start: Cell<f64>,
    pub request_start: Cell<f64>,
    pub response_start: Cell<f64>,
    pub response_end: Cell<f64>,
    pub dom_interactive: Cell<f64>,
    pub dom_content_loaded_start: Cell<f64>,
    pub dom_content_loaded_end: Cell<f64>,
    pub dom_complete: Cell<f64>,
    pub load_start: Cell<f64>,
    pub load_end: Cell<f64>,
    /// Bytes of the document as received and as decoded.
    pub encoded_size: Cell<u64>,
    pub decoded_size: Cell<u64>,
}

/// The fields of the navigation entry, in the order they are reported.
fn fields(page: &PageState) -> Vec<(&'static str, Value)> {
    let t = &page.timing;
    let ms = |cell: &Cell<f64>| Value::Number(cell.get());
    vec![
        ("name", Value::String(page.url.borrow().to_string())),
        ("entryType", Value::String("navigation".to_string())),
        ("startTime", Value::Number(0.0)),
        ("duration", ms(&t.load_end)),
        ("initiatorType", Value::String("navigation".to_string())),
        ("deliveryType", Value::String(String::new())),
        ("nextHopProtocol", Value::String(String::new())),
        ("workerStart", Value::Number(0.0)),
        ("redirectStart", Value::Number(0.0)),
        ("redirectEnd", Value::Number(0.0)),
        ("fetchStart", ms(&t.fetch_start)),
        ("domainLookupStart", ms(&t.fetch_start)),
        ("domainLookupEnd", ms(&t.fetch_start)),
        ("connectStart", ms(&t.fetch_start)),
        ("connectEnd", ms(&t.fetch_start)),
        ("secureConnectionStart", Value::Number(0.0)),
        ("requestStart", ms(&t.request_start)),
        ("responseStart", ms(&t.response_start)),
        ("responseEnd", ms(&t.response_end)),
        ("transferSize", Value::Number(t.encoded_size.get() as f64)),
        (
            "encodedBodySize",
            Value::Number(t.encoded_size.get() as f64),
        ),
        (
            "decodedBodySize",
            Value::Number(t.decoded_size.get() as f64),
        ),
        ("unloadEventStart", Value::Number(0.0)),
        ("unloadEventEnd", Value::Number(0.0)),
        ("domInteractive", ms(&t.dom_interactive)),
        (
            "domContentLoadedEventStart",
            ms(&t.dom_content_loaded_start),
        ),
        ("domContentLoadedEventEnd", ms(&t.dom_content_loaded_end)),
        ("domComplete", ms(&t.dom_complete)),
        ("loadEventStart", ms(&t.load_start)),
        ("loadEventEnd", ms(&t.load_end)),
        ("type", Value::String("navigate".to_string())),
        ("redirectCount", Value::Number(0.0)),
    ]
}

fn field(page: &PageState, name: &str) -> f64 {
    fields(page)
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| match v {
            Value::Number(n) => n,
            _ => 0.0,
        })
        .unwrap_or(0.0)
}

/// `performance.timing`.
pub struct TimingObject;
platform_object!(TimingObject, PerformanceTiming);

/// `performance.navigation`.
pub struct NavigationObject;
platform_object!(NavigationObject, PerformanceNavigation);

fn check<T: crate::PlatformObject>(cx: &Cx<'_>, this: ObjectId) -> Fallible<()> {
    cx.page.with::<T, _>(this, |_| ())
}

macro_rules! resource_fields {
    ($($method:ident => $name:literal),* $(,)?) => {
        impl web::PerformanceResourceTimingImpl for Web {
            fn initiator_type(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
                crate::performance::is_navigation_entry(cx, this)?;
                Ok("navigation".to_string())
            }

            fn delivery_type(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
                crate::performance::is_navigation_entry(cx, this)?;
                Ok(String::new())
            }

            fn next_hop_protocol(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
                crate::performance::is_navigation_entry(cx, this)?;
                Ok(String::new())
            }

            fn transfer_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u64> {
                crate::performance::is_navigation_entry(cx, this)?;
                Ok(cx.page.timing.encoded_size.get())
            }

            fn encoded_body_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u64> {
                crate::performance::is_navigation_entry(cx, this)?;
                Ok(cx.page.timing.encoded_size.get())
            }

            fn decoded_body_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u64> {
                crate::performance::is_navigation_entry(cx, this)?;
                Ok(cx.page.timing.decoded_size.get())
            }

            fn to_json(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
                crate::performance::is_navigation_entry(cx, this)?;
                Ok(Value::Record(
                    fields(cx.page)
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                ))
            }

            $(
                fn $method(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
                    crate::performance::is_navigation_entry(cx, this)?;
                    Ok(field(cx.page, $name))
                }
            )*
        }
    };
}

resource_fields! {
    worker_start => "workerStart",
    redirect_start => "redirectStart",
    redirect_end => "redirectEnd",
    fetch_start => "fetchStart",
    domain_lookup_start => "domainLookupStart",
    domain_lookup_end => "domainLookupEnd",
    connect_start => "connectStart",
    connect_end => "connectEnd",
    secure_connection_start => "secureConnectionStart",
    request_start => "requestStart",
    response_start => "responseStart",
    response_end => "responseEnd",
}

macro_rules! navigation_fields {
    ($($method:ident => $name:literal),* $(,)?) => {
        impl web::PerformanceNavigationTimingImpl for Web {
            fn type_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NavigationTimingType> {
                crate::performance::is_navigation_entry(cx, this)?;
                Ok(NavigationTimingType::Navigate)
            }

            fn redirect_count(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
                crate::performance::is_navigation_entry(cx, this)?;
                Ok(0)
            }

            fn to_json(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
                <Web as web::PerformanceResourceTimingImpl>::to_json(cx, this)
            }

            $(
                fn $method(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
                    crate::performance::is_navigation_entry(cx, this)?;
                    Ok(field(cx.page, $name))
                }
            )*
        }
    };
}

navigation_fields! {
    unload_event_start => "unloadEventStart",
    unload_event_end => "unloadEventEnd",
    dom_interactive => "domInteractive",
    dom_content_loaded_event_start => "domContentLoadedEventStart",
    dom_content_loaded_event_end => "domContentLoadedEventEnd",
    dom_complete => "domComplete",
    load_event_start => "loadEventStart",
    load_event_end => "loadEventEnd",
}

/// A milestone as the legacy interface gives it: milliseconds since the
/// epoch, or 0 while not reached.
fn epoch(page: &PageState, name: &str) -> u64 {
    let relative = field(page, name);
    if relative <= 0.0 && name != "navigationStart" {
        return 0;
    }
    (page.clock.time_origin() + relative).round() as u64
}

const TIMING_FIELDS: &[(&str, &str)] = &[
    ("navigationStart", "startTime"),
    ("unloadEventStart", "unloadEventStart"),
    ("unloadEventEnd", "unloadEventEnd"),
    ("redirectStart", "redirectStart"),
    ("redirectEnd", "redirectEnd"),
    ("fetchStart", "fetchStart"),
    ("domainLookupStart", "domainLookupStart"),
    ("domainLookupEnd", "domainLookupEnd"),
    ("connectStart", "connectStart"),
    ("connectEnd", "connectEnd"),
    ("secureConnectionStart", "secureConnectionStart"),
    ("requestStart", "requestStart"),
    ("responseStart", "responseStart"),
    ("responseEnd", "responseEnd"),
    ("domLoading", "responseStart"),
    ("domInteractive", "domInteractive"),
    ("domContentLoadedEventStart", "domContentLoadedEventStart"),
    ("domContentLoadedEventEnd", "domContentLoadedEventEnd"),
    ("domComplete", "domComplete"),
    ("loadEventStart", "loadEventStart"),
    ("loadEventEnd", "loadEventEnd"),
];

macro_rules! timing_fields {
    ($($method:ident => $name:literal),* $(,)?) => {
        impl web::PerformanceTimingImpl for Web {
            fn to_json(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
                check::<TimingObject>(cx, this)?;
                Ok(Value::Record(
                    TIMING_FIELDS
                        .iter()
                        .map(|(name, source)| {
                            let value = if *name == "navigationStart" {
                                page_epoch(cx.page, "navigationStart")
                            } else {
                                epoch(cx.page, source)
                            };
                            (name.to_string(), Value::Number(value as f64))
                        })
                        .collect(),
                ))
            }

            $(
                fn $method(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u64> {
                    check::<TimingObject>(cx, this)?;
                    Ok(if $name == "navigationStart" {
                        page_epoch(cx.page, "navigationStart")
                    } else {
                        let source = TIMING_FIELDS
                            .iter()
                            .find(|(n, _)| *n == $name)
                            .map_or($name, |(_, s)| *s);
                        epoch(cx.page, source)
                    })
                }
            )*
        }
    };
}

fn page_epoch(page: &PageState, _name: &str) -> u64 {
    page.clock.time_origin().round() as u64
}

timing_fields! {
    navigation_start => "navigationStart",
    unload_event_start => "unloadEventStart",
    unload_event_end => "unloadEventEnd",
    redirect_start => "redirectStart",
    redirect_end => "redirectEnd",
    fetch_start => "fetchStart",
    domain_lookup_start => "domainLookupStart",
    domain_lookup_end => "domainLookupEnd",
    connect_start => "connectStart",
    connect_end => "connectEnd",
    secure_connection_start => "secureConnectionStart",
    request_start => "requestStart",
    response_start => "responseStart",
    response_end => "responseEnd",
    dom_loading => "domLoading",
    dom_interactive => "domInteractive",
    dom_content_loaded_event_start => "domContentLoadedEventStart",
    dom_content_loaded_event_end => "domContentLoadedEventEnd",
    dom_complete => "domComplete",
    load_event_start => "loadEventStart",
    load_event_end => "loadEventEnd",
}

impl web::PerformanceNavigationImpl for Web {
    fn type_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        check::<NavigationObject>(cx, this)?;
        Ok(0)
    }

    fn redirect_count(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        check::<NavigationObject>(cx, this)?;
        Ok(0)
    }

    fn to_json(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        check::<NavigationObject>(cx, this)?;
        Ok(Value::Record(vec![
            ("type".to_string(), Value::Number(0.0)),
            ("redirectCount".to_string(), Value::Number(0.0)),
        ]))
    }
}

/// Records that a milestone was reached now, unless it already was.
pub(crate) fn reached(page: &PageState, milestone: &Cell<f64>) {
    if milestone.get() <= 0.0 {
        milestone.set(page.clock.now().max(f64::MIN_POSITIVE));
    }
}
