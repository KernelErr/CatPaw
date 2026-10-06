//! User timing and the performance timeline: `performance.mark()` and
//! `measure()`, the entries they leave, and `PerformanceObserver`.
//!
//! Marks and measures are the only entries there are. The page does not
//! time its loading, its resources or its rendering yet, and says so
//! through `PerformanceObserver.supportedEntryTypes`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use catpaw_js::{Callback, Exception, Fallible, ObjectId, Value};

use crate::event_loop;
use crate::generated::{self as web, InterfaceId, StringOrDouble};
use crate::page::{Cx, PageState};
use crate::{Web, platform_object};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mark,
    Measure,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Mark => "mark",
            Kind::Measure => "measure",
        }
    }

    fn parse(entry_type: &str) -> Option<Self> {
        match entry_type {
            "mark" => Some(Kind::Mark),
            "measure" => Some(Kind::Measure),
            _ => None,
        }
    }
}

/// What an entry records. The timeline, observer buffers and entry lists
/// share entries; the script objects for them are made on demand.
struct Entry {
    kind: Kind,
    name: String,
    start: f64,
    duration: f64,
    detail: Value,
}

pub struct EntryObject {
    entry: Rc<Entry>,
}
platform_object!(EntryObject, |e| match e.entry.kind {
    Kind::Mark => InterfaceId::PerformanceMark,
    Kind::Measure => InterfaceId::PerformanceMeasure,
});

pub struct ObserverObject {
    callback: Callback,
    /// Whether it observes one `type` at a time rather than `entryTypes`;
    /// settled by the first call to `observe()`.
    single: Option<bool>,
    types: Vec<Kind>,
    /// Entries waiting to be delivered.
    buffer: Vec<Rc<Entry>>,
    /// The next callback is told how many entries were dropped.
    reports_dropped: bool,
}
platform_object!(ObserverObject, PerformanceObserver);

pub struct EntryListObject {
    entries: Vec<Rc<Entry>>,
}
platform_object!(EntryListObject, PerformanceObserverEntryList);

/// The page's performance timeline.
#[derive(Default)]
pub(crate) struct Timeline {
    entries: RefCell<Vec<Rc<Entry>>>,
    /// The script object of an entry, by the entry's address.
    objects: RefCell<HashMap<usize, ObjectId>>,
    /// The size at which objects that are gone are next weeded out.
    prune_at: Cell<usize>,
    /// The observers that observe something, in the order they began.
    observers: RefCell<Vec<ObjectId>>,
    /// The task that notifies them is queued.
    task_queued: Cell<bool>,
}

/// The script object for `entry`: the same one for as long as script holds
/// on to it.
fn object_for(page: &PageState, entry: &Rc<Entry>) -> ObjectId {
    let key = Rc::as_ptr(entry) as usize;
    let timeline = &page.timeline;
    let known = timeline.objects.borrow().get(&key).copied();
    let current = known.filter(|&id| {
        page.try_with::<EntryObject, _>(id, |e| Rc::ptr_eq(&e.entry, entry)) == Some(true)
    });
    if let Some(id) = current {
        return id;
    }
    let id = page.alloc(EntryObject {
        entry: entry.clone(),
    });
    let mut objects = timeline.objects.borrow_mut();
    if objects.len() >= timeline.prune_at.get() {
        objects.retain(|_, id| page.interface_of(*id).is_some());
        timeline.prune_at.set((objects.len() * 2).max(256));
    }
    objects.insert(key, id);
    id
}

/// The entries of `entries` with the given name and type, in the order
/// they started.
fn select(
    page: &PageState,
    entries: &[Rc<Entry>],
    name: Option<&str>,
    entry_type: Option<&str>,
) -> Vec<ObjectId> {
    let kind = match entry_type.map(Kind::parse) {
        None => None,
        Some(Some(kind)) => Some(kind),
        // A type no entry has.
        Some(None) => return Vec::new(),
    };
    let mut selected: Vec<&Rc<Entry>> = entries
        .iter()
        .filter(|e| name.is_none_or(|n| e.name == n) && kind.is_none_or(|k| e.kind == k))
        .collect();
    selected.sort_by(|a, b| a.start.total_cmp(&b.start));
    selected
        .into_iter()
        .map(|entry| object_for(page, entry))
        .collect()
}

fn schedule(page: &PageState) {
    if !page.timeline.task_queued.replace(true) {
        event_loop::queue_task(page, "performance observers", notify);
    }
}

/// <https://w3c.github.io/performance-timeline/#queue-a-performanceentry>
fn queue(page: &PageState, entry: Rc<Entry>) {
    let observers = page.timeline.observers.borrow().clone();
    let mut interested = false;
    for observer in observers {
        let added = page.try_with::<ObserverObject, _>(observer, |o| {
            let observes = o.types.contains(&entry.kind);
            if observes {
                o.buffer.push(entry.clone());
            }
            observes
        });
        interested |= added == Some(true);
    }
    page.timeline.entries.borrow_mut().push(entry);
    if interested {
        schedule(page);
    }
}

fn notify(cx: &mut Cx<'_>) {
    let page = cx.page;
    page.timeline.task_queued.set(false);
    let observers = page.timeline.observers.borrow().clone();
    for observer in observers {
        let taken = page.try_with::<ObserverObject, _>(observer, |o| {
            let reports_dropped = std::mem::take(&mut o.reports_dropped);
            (
                o.callback.clone(),
                std::mem::take(&mut o.buffer),
                reports_dropped,
            )
        });
        let Some((callback, entries, reports_dropped)) = taken else {
            continue;
        };
        if entries.is_empty() {
            continue;
        }
        let list = Value::Object(page.alloc(EntryListObject { entries }));
        // Nothing is ever dropped: the buffers have no limit.
        let options = Value::Record(if reports_dropped {
            vec![("droppedEntriesCount".to_string(), Value::Number(0.0))]
        } else {
            Vec::new()
        });
        let this = Value::Object(observer);
        if let Err(e) = cx
            .script
            .call(&callback, &this, &[list, this.clone(), options])
        {
            cx.report_exception(&e);
        }
    }
}

/// The names `PerformanceTiming` gave to moments of the navigation, which
/// marks may not reuse.
const NAVIGATION_TIMING_NAMES: &[&str] = &[
    "navigationStart",
    "unloadEventStart",
    "unloadEventEnd",
    "redirectStart",
    "redirectEnd",
    "fetchStart",
    "domainLookupStart",
    "domainLookupEnd",
    "connectStart",
    "connectEnd",
    "secureConnectionStart",
    "requestStart",
    "responseStart",
    "responseEnd",
    "domLoading",
    "domInteractive",
    "domContentLoadedEventStart",
    "domContentLoadedEventEnd",
    "domComplete",
    "loadEventStart",
    "loadEventEnd",
];

/// The copy of `detail` an entry keeps.
fn clone_detail(cx: &mut Cx<'_>, detail: &Value) -> Fallible<Value> {
    match detail {
        Value::Undefined | Value::Null => Ok(Value::Null),
        other => cx.script.structured_clone(other),
    }
}

fn new_mark(
    cx: &mut Cx<'_>,
    name: String,
    options: web::PerformanceMarkOptions,
) -> Fallible<Rc<Entry>> {
    if NAVIGATION_TIMING_NAMES.contains(&name.as_str()) {
        return Err(Exception::syntax(format!(
            "'{name}' is part of the navigation timing and cannot be used as a mark name"
        )));
    }
    let start = match options.start_time {
        Some(time) if time < 0.0 => {
            return Err(Exception::type_error("A mark cannot start before time 0"));
        }
        Some(time) => time,
        None => cx.page.clock.now(),
    };
    Ok(Rc::new(Entry {
        kind: Kind::Mark,
        name,
        start,
        duration: 0.0,
        detail: clone_detail(cx, &options.detail)?,
    }))
}

/// <https://w3c.github.io/user-timing/#convert-a-mark-to-a-timestamp>
fn timestamp(page: &PageState, mark: &StringOrDouble) -> Fallible<f64> {
    match mark {
        StringOrDouble::Double(time) if *time < 0.0 => {
            Err(Exception::type_error("A timestamp cannot be negative"))
        }
        StringOrDouble::Double(time) => Ok(*time),
        // The navigation starts at the time origin; its other moments are
        // not timed.
        StringOrDouble::String(name) if name == "navigationStart" => Ok(0.0),
        StringOrDouble::String(name) if NAVIGATION_TIMING_NAMES.contains(&name.as_str()) => Err(
            Exception::invalid_access(format!("The navigation timing has no value for '{name}'")),
        ),
        StringOrDouble::String(name) => {
            let entries = page.timeline.entries.borrow();
            let mark = entries
                .iter()
                .rev()
                .find(|e| e.kind == Kind::Mark && e.name == *name);
            mark.map(|e| e.start)
                .ok_or_else(|| Exception::syntax(format!("There is no mark named '{name}'")))
        }
    }
}

pub(crate) fn mark(
    cx: &mut Cx<'_>,
    name: String,
    options: web::PerformanceMarkOptions,
) -> Fallible<ObjectId> {
    let entry = new_mark(cx, name, options)?;
    queue(cx.page, entry.clone());
    Ok(object_for(cx.page, &entry))
}

/// <https://w3c.github.io/user-timing/#dom-performance-measure>
pub(crate) fn measure(
    cx: &mut Cx<'_>,
    name: String,
    start_or_options: web::StringOrPerformanceMeasureOptions,
    end_mark: Option<String>,
) -> Fallible<ObjectId> {
    let page = cx.page;
    let (start_mark, options) = match start_or_options {
        web::StringOrPerformanceMeasureOptions::String(mark) => (Some(mark), None),
        web::StringOrPerformanceMeasureOptions::PerformanceMeasureOptions(options) => {
            let given = options.start.is_some()
                || options.end.is_some()
                || options.duration.is_some()
                || !matches!(options.detail, Value::Undefined);
            (None, given.then_some(options))
        }
    };
    if let Some(options) = &options {
        if end_mark.is_some() {
            return Err(Exception::type_error(
                "An end mark cannot be given together with measure options",
            ));
        }
        if options.start.is_none() && options.end.is_none() {
            return Err(Exception::type_error(
                "Measure options need a start or an end",
            ));
        }
        if options.start.is_some() && options.end.is_some() && options.duration.is_some() {
            return Err(Exception::type_error(
                "Measure options cannot give a start, an end and a duration",
            ));
        }
    }
    let start_option = options.as_ref().and_then(|o| o.start.as_ref());
    let end_option = options.as_ref().and_then(|o| o.end.as_ref());
    let duration = options.as_ref().and_then(|o| o.duration);

    let end = if let Some(mark) = end_mark {
        timestamp(page, &StringOrDouble::String(mark))?
    } else if let Some(end) = end_option {
        timestamp(page, end)?
    } else if let (Some(start), Some(duration)) = (start_option, duration) {
        timestamp(page, start)? + duration
    } else {
        page.clock.now()
    };
    let start = if let Some(start) = start_option {
        timestamp(page, start)?
    } else if let (Some(_), Some(duration)) = (end_option, duration) {
        end - duration
    } else if let Some(mark) = start_mark {
        timestamp(page, &StringOrDouble::String(mark))?
    } else {
        0.0
    };
    let detail = match &options {
        Some(options) => clone_detail(cx, &options.detail)?,
        None => Value::Null,
    };
    let entry = Rc::new(Entry {
        kind: Kind::Measure,
        name,
        start,
        duration: end - start,
        detail,
    });
    queue(page, entry.clone());
    Ok(object_for(page, &entry))
}

/// `clearMarks()` and `clearMeasures()`.
pub(crate) fn clear(page: &PageState, measures: bool, name: Option<&str>) {
    let kind = if measures { Kind::Measure } else { Kind::Mark };
    page.timeline
        .entries
        .borrow_mut()
        .retain(|e| e.kind != kind || name.is_some_and(|n| e.name != n));
}

/// `performance.getEntries()` and its filtering relatives.
pub(crate) fn entries(
    page: &PageState,
    name: Option<&str>,
    entry_type: Option<&str>,
) -> Vec<ObjectId> {
    let entries = page.timeline.entries.borrow().clone();
    select(page, &entries, name, entry_type)
}

fn entry<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&Entry) -> R) -> Fallible<R> {
    cx.page.with::<EntryObject, _>(this, |e| f(&e.entry))
}

impl web::PerformanceEntryImpl for Web {
    fn name(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        entry(cx, this, |e| e.name.clone())
    }

    fn entry_type(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        entry(cx, this, |e| e.kind.as_str().to_string())
    }

    fn start_time(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        entry(cx, this, |e| e.start)
    }

    fn duration(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        entry(cx, this, |e| e.duration)
    }

    fn to_json(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        entry(cx, this, |e| {
            Value::Record(vec![
                ("name".to_string(), Value::String(e.name.clone())),
                (
                    "entryType".to_string(),
                    Value::String(e.kind.as_str().to_string()),
                ),
                ("startTime".to_string(), Value::Number(e.start)),
                ("duration".to_string(), Value::Number(e.duration)),
                ("detail".to_string(), e.detail.clone()),
            ])
        })
    }
}

impl web::PerformanceMarkImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        mark_name: String,
        mark_options: web::PerformanceMarkOptions,
    ) -> Fallible<ObjectId> {
        // A mark made this way is not on the timeline.
        let entry = new_mark(cx, mark_name, mark_options)?;
        Ok(object_for(cx.page, &entry))
    }

    fn detail(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        entry(cx, this, |e| e.detail.clone())
    }
}

impl web::PerformanceMeasureImpl for Web {
    fn detail(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        entry(cx, this, |e| e.detail.clone())
    }
}

fn observer<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut ObserverObject) -> R,
) -> Fallible<R> {
    cx.page.with::<ObserverObject, _>(this, f)
}

impl web::PerformanceObserverImpl for Web {
    fn constructor(cx: &mut Cx<'_>, callback: Callback) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(ObserverObject {
            callback,
            single: None,
            types: Vec::new(),
            buffer: Vec::new(),
            reports_dropped: false,
        }))
    }

    fn observe(
        cx: &mut Cx<'_>,
        this: ObjectId,
        options: web::PerformanceObserverInit,
    ) -> Fallible<()> {
        let page = cx.page;
        if options.entry_types.is_none() && options.type_.is_none() {
            return Err(Exception::type_error(
                "observe() needs either 'entryTypes' or 'type'",
            ));
        }
        if options.entry_types.is_some() && (options.type_.is_some() || options.buffered.is_some())
        {
            return Err(Exception::type_error(
                "'entryTypes' cannot be combined with other options",
            ));
        }
        let single = options.type_.is_some();
        let consistent = observer(cx, this, |o| *o.single.get_or_insert(single) == single)?;
        if !consistent {
            return Err(Exception::invalid_modification(
                "An observer observes either with 'entryTypes' or with 'type', not both",
            ));
        }

        // Types that nothing is recorded for are passed over.
        let buffered: Vec<Rc<Entry>> = if let Some(entry_types) = &options.entry_types {
            let mut kinds: Vec<Kind> = Vec::new();
            for kind in entry_types.iter().filter_map(|t| Kind::parse(t)) {
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
            if kinds.is_empty() {
                return Ok(());
            }
            observer(cx, this, |o| o.types = kinds)?;
            Vec::new()
        } else {
            let Some(kind) = options.type_.as_deref().and_then(Kind::parse) else {
                return Ok(());
            };
            observer(cx, this, |o| {
                if !o.types.contains(&kind) {
                    o.types.push(kind);
                }
            })?;
            if options.buffered == Some(true) {
                let entries = page.timeline.entries.borrow();
                entries.iter().filter(|e| e.kind == kind).cloned().collect()
            } else {
                Vec::new()
            }
        };

        let registered = {
            let mut observers = page.timeline.observers.borrow_mut();
            let registered = observers.contains(&this);
            if !registered {
                observers.push(this);
            }
            registered
        };
        if !registered {
            // An observer is kept alive by what it observes.
            cx.pin(this);
        }
        observer(cx, this, |o| {
            o.reports_dropped = true;
            o.buffer.extend(buffered);
        })?;
        if options.buffered == Some(true) {
            schedule(page);
        }
        Ok(())
    }

    fn disconnect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        observer(cx, this, |o| {
            o.types.clear();
            o.buffer.clear();
            o.single = None;
        })?;
        let registered = {
            let mut observers = cx.page.timeline.observers.borrow_mut();
            let before = observers.len();
            observers.retain(|o| *o != this);
            observers.len() != before
        };
        if registered {
            cx.unpin(this);
        }
        Ok(())
    }

    fn take_records(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<ObjectId>> {
        let entries = observer(cx, this, |o| std::mem::take(&mut o.buffer))?;
        Ok(select(cx.page, &entries, None, None))
    }

    fn supported_entry_types(_cx: &mut Cx<'_>) -> Fallible<Vec<String>> {
        Ok(vec![
            Kind::Mark.as_str().to_string(),
            Kind::Measure.as_str().to_string(),
        ])
    }
}

fn listed(
    cx: &Cx<'_>,
    this: ObjectId,
    name: Option<&str>,
    entry_type: Option<&str>,
) -> Fallible<Vec<ObjectId>> {
    let entries = cx
        .page
        .with::<EntryListObject, _>(this, |list| list.entries.clone())?;
    Ok(select(cx.page, &entries, name, entry_type))
}

impl web::PerformanceObserverEntryListImpl for Web {
    fn get_entries(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<ObjectId>> {
        listed(cx, this, None, None)
    }

    fn get_entries_by_type(
        cx: &mut Cx<'_>,
        this: ObjectId,
        type_: String,
    ) -> Fallible<Vec<ObjectId>> {
        listed(cx, this, None, Some(&type_))
    }

    fn get_entries_by_name(
        cx: &mut Cx<'_>,
        this: ObjectId,
        name: String,
        type_: Option<String>,
    ) -> Fallible<Vec<ObjectId>> {
        listed(cx, this, Some(&name), type_.as_deref())
    }
}
