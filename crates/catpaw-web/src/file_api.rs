//! The File API (`Blob`, `File`, `FileReader`, object URLs) and `FormData`.
//!
//! A blob is bytes in memory with a type. There are no file inputs yet, so
//! the files there are come from script. Object URLs (`blob:`) resolve
//! inside the page, through `net::local_response`, like `data:` URLs.
//! `FormData` keeps its entries; `new FormData(form)` constructs the entry
//! list from the form's inputs, textareas and selects (whose selectedness
//! is read from the `selected` attribute until forms track it), and a body
//! made from one is `multipart/form-data`.

use std::collections::HashMap;
use std::rc::Rc;

use base64::Engine as _;
use catpaw_dom::{Dom, NodeId};
use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId, PromiseRef, Value};

use crate::element::is_checked;
use crate::generated::{
    self as web, BlobPropertyBag, BufferSourceOrBlobOrString as BlobPart, EndingType, FileOrString,
    FilePropertyBag, InterfaceId, StringOrArrayBuffer,
};
use crate::page::{Cx, PageState};
use crate::{Web, events, node, platform_object, streams};

// ------------------------------------------------------------------ Blob

struct FileInfo {
    name: String,
    last_modified: i64,
}

pub struct BlobObject {
    bytes: Rc<Vec<u8>>,
    type_: String,
    file: Option<FileInfo>,
}
platform_object!(BlobObject, |b| if b.file.is_some() {
    InterfaceId::File
} else {
    InterfaceId::Blob
});

/// A `type` as a blob keeps it: lowercase, or empty if it has characters
/// outside printable ASCII.
fn normalize_type(t: &str) -> String {
    if t.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        t.to_ascii_lowercase()
    } else {
        String::new()
    }
}

fn blob<R>(cx: &Cx<'_>, id: ObjectId, f: impl FnOnce(&mut BlobObject) -> R) -> Fallible<R> {
    cx.page.with::<BlobObject, _>(id, f)
}

/// A blob's bytes and type.
pub(crate) fn blob_contents(cx: &Cx<'_>, id: ObjectId) -> Fallible<(Rc<Vec<u8>>, String)> {
    blob(cx, id, |b| (b.bytes.clone(), b.type_.clone()))
}

/// Makes a blob of `bytes`.
pub(crate) fn new_blob(cx: &Cx<'_>, bytes: Vec<u8>, type_: &str) -> ObjectId {
    cx.page.alloc(BlobObject {
        bytes: Rc::new(bytes),
        type_: normalize_type(type_),
        file: None,
    })
}

fn new_file(cx: &Cx<'_>, bytes: Vec<u8>, type_: &str, name: &str, last_modified: i64) -> ObjectId {
    cx.page.alloc(BlobObject {
        bytes: Rc::new(bytes),
        type_: normalize_type(type_),
        file: Some(FileInfo {
            name: name.to_string(),
            last_modified,
        }),
    })
}

// -------------------------------------------------------------- FileList

/// What a file input chose (`input.files`).
pub struct FileListObject {
    /// Pinned for as long as the page lives: script may hold the list.
    files: Vec<ObjectId>,
}
platform_object!(FileListObject, FileList);

impl web::FileListImpl for Web {
    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<ObjectId>> {
        cx.page
            .with::<FileListObject, _>(this, |l| l.files.get(index as usize).copied())
    }

    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        cx.page
            .with::<FileListObject, _>(this, |l| l.files.len() as u32)
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<ObjectId>> {
        Self::item(cx, this, index)
    }
}

/// The `FileList` of a file input: the same object until the choice
/// changes.
pub(crate) fn file_list(cx: &mut Cx<'_>, input: NodeId) -> ObjectId {
    if let Some(&list) = cx.page.file_lists.borrow().get(&input) {
        return list;
    }
    let list = cx.page.alloc(FileListObject { files: Vec::new() });
    cx.pin(list);
    cx.page.file_lists.borrow_mut().insert(input, list);
    list
}

/// Makes `list` (a `FileList`, or nothing for none) what a file input
/// chose.
pub(crate) fn set_file_list(cx: &mut Cx<'_>, input: NodeId, list: Option<ObjectId>) {
    let list = list.unwrap_or_else(|| cx.page.alloc(FileListObject { files: Vec::new() }));
    cx.pin(list);
    let old = cx.page.file_lists.borrow_mut().insert(input, list);
    if let Some(old) = old {
        cx.unpin(old);
    }
}

/// Forgets what an input chose, as it stops being a file input.
pub(crate) fn forget_files(cx: &mut Cx<'_>, input: NodeId) {
    let list = cx.page.file_lists.borrow_mut().remove(&input);
    if let Some(list) = list {
        cx.unpin(list);
    }
}

/// The files a file input chose.
pub(crate) fn chosen_files(cx: &Cx<'_>, input: NodeId) -> Vec<ObjectId> {
    let list = cx.page.file_lists.borrow().get(&input).copied();
    list.and_then(|list| {
        cx.page
            .with::<FileListObject, _>(list, |l| l.files.clone())
            .ok()
    })
    .unwrap_or_default()
}

/// The name of a file.
pub(crate) fn file_name(cx: &Cx<'_>, file: ObjectId) -> Option<String> {
    blob(cx, file, |b| b.file.as_ref().map(|f| f.name.clone()))
        .ok()
        .flatten()
}

/// Chooses files in a file input, as its file picker would: each is a
/// name, a type and the bytes. The input then fires `input` and `change`
/// (the caller's part, as a user's choice).
pub(crate) fn choose_files(cx: &mut Cx<'_>, input: NodeId, files: Vec<(String, String, Vec<u8>)>) {
    let now = now_ms(cx);
    let ids: Vec<ObjectId> = files
        .into_iter()
        .map(|(name, type_, bytes)| {
            let id = new_file(cx, bytes, &type_, &name, now);
            cx.pin(id);
            id
        })
        .collect();
    let list = cx.page.alloc(FileListObject { files: ids });
    set_file_list(cx, input, Some(list));
}

/// The current time as the page sees it (`Date.now()` agrees).
fn now_ms(cx: &Cx<'_>) -> i64 {
    cx.page.clock.unix_ms().floor() as i64
}

/// The bytes of blob parts, with line endings converted when asked.
fn concat_parts(cx: &Cx<'_>, parts: Vec<BlobPart>, endings: EndingType) -> Fallible<Vec<u8>> {
    let mut out = Vec::new();
    for part in parts {
        match part {
            BlobPart::BufferSource(bytes) => out.extend(bytes),
            BlobPart::Blob(id) => out.extend(blob_contents(cx, id)?.0.iter()),
            BlobPart::String(text) => match endings {
                EndingType::Native => {
                    let native = if cfg!(windows) { "\r\n" } else { "\n" };
                    out.extend(
                        text.replace("\r\n", "\n")
                            .replace('\r', "\n")
                            .replace('\n', native)
                            .into_bytes(),
                    );
                }
                EndingType::Transparent => out.extend(text.into_bytes()),
            },
        }
    }
    Ok(out)
}

/// `Blob.slice()`'s index rules: negative from the end, clamped.
fn slice_bounds(len: usize, start: Option<i64>, end: Option<i64>) -> (usize, usize) {
    let len_i = len as i64;
    let clamp = |i: i64| -> usize {
        if i < 0 {
            (len_i + i).max(0) as usize
        } else {
            i.min(len_i) as usize
        }
    };
    let start = start.map_or(0, clamp);
    let end = end.map_or(len, clamp);
    (start, end.max(start))
}

fn resolved(cx: &mut Cx<'_>, value: Value) -> PromiseRef {
    let promise = cx.script.new_promise();
    cx.script.resolve_promise(&promise, value);
    promise
}

impl web::BlobImpl for Web {
    fn size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u64> {
        blob(cx, this, |b| b.bytes.len() as u64)
    }

    fn type_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        blob(cx, this, |b| b.type_.clone())
    }

    fn slice(
        cx: &mut Cx<'_>,
        this: ObjectId,
        start: Option<i64>,
        end: Option<i64>,
        content_type: Option<String>,
    ) -> Fallible<ObjectId> {
        let (bytes, _) = blob_contents(cx, this)?;
        let (from, to) = slice_bounds(bytes.len(), start, end);
        let part = bytes[from..to].to_vec();
        Ok(new_blob(cx, part, content_type.as_deref().unwrap_or("")))
    }

    fn stream(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let (bytes, _) = blob_contents(cx, this)?;
        streams::readable_from_bytes(cx, bytes.to_vec())
    }

    fn text(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        let (bytes, _) = blob_contents(cx, this)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(resolved(cx, Value::String(text)))
    }

    fn array_buffer(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        let (bytes, _) = blob_contents(cx, this)?;
        Ok(resolved(cx, Value::ArrayBuffer(bytes.to_vec())))
    }

    fn bytes(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        let (bytes, _) = blob_contents(cx, this)?;
        Ok(resolved(cx, Value::Uint8Array(bytes.to_vec())))
    }

    fn constructor(
        cx: &mut Cx<'_>,
        blob_parts: Option<Vec<BlobPart>>,
        options: BlobPropertyBag,
    ) -> Fallible<ObjectId> {
        let bytes = concat_parts(cx, blob_parts.unwrap_or_default(), options.endings)?;
        Ok(new_blob(cx, bytes, &options.type_))
    }
}

impl web::FileImpl for Web {
    fn name(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        blob(cx, this, |b| {
            b.file.as_ref().map(|f| f.name.clone()).unwrap_or_default()
        })
    }

    fn last_modified(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i64> {
        blob(cx, this, |b| b.file.as_ref().map_or(0, |f| f.last_modified))
    }

    fn constructor(
        cx: &mut Cx<'_>,
        file_bits: Vec<BlobPart>,
        file_name: String,
        options: FilePropertyBag,
    ) -> Fallible<ObjectId> {
        let bytes = concat_parts(cx, file_bits, options.endings)?;
        let last_modified = options.last_modified.unwrap_or_else(|| now_ms(cx));
        Ok(new_file(
            cx,
            bytes,
            &options.type_,
            &file_name,
            last_modified,
        ))
    }
}

// ------------------------------------------------------------ object URLs

/// The page's object URLs: what `blob:` URLs resolve to.
#[derive(Default)]
pub struct BlobUrls {
    entries: HashMap<String, (Rc<Vec<u8>>, String)>,
}

impl BlobUrls {
    /// The bytes and type behind a `blob:` URL, if the page made it.
    pub fn get(&self, url: &str) -> Option<(Rc<Vec<u8>>, String)> {
        self.entries.get(url).cloned()
    }
}

/// `URL.createObjectURL()`.
pub(crate) fn create_object_url(cx: &mut Cx<'_>, blob_id: ObjectId) -> Fallible<String> {
    let (bytes, type_) = blob_contents(cx, blob_id)?;
    let origin = cx.page.url.borrow().origin().ascii_serialization();
    let url = format!("blob:{origin}/{}", crate::crypto::uuid_v4()?);
    cx.page
        .blob_urls
        .borrow_mut()
        .entries
        .insert(url.clone(), (bytes, type_));
    Ok(url)
}

/// `URL.revokeObjectURL()`.
pub(crate) fn revoke_object_url(page: &PageState, url: &str) {
    page.blob_urls.borrow_mut().entries.remove(url);
}

// -------------------------------------------------------------- FileReader

const EMPTY: u16 = 0;
const LOADING: u16 = 1;
const DONE: u16 = 2;

pub struct FileReaderObject {
    state: u16,
    result: Option<StringOrArrayBuffer>,
    error: Value,
}
platform_object!(FileReaderObject, FileReader);

enum ReadAs {
    ArrayBuffer,
    BinaryString,
    Text(Option<String>),
    DataUrl,
}

fn decode_text(bytes: &[u8], label: Option<&str>) -> String {
    let encoding = label
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

/// <https://w3c.github.io/FileAPI/#readOperation>, with the whole blob
/// read in one task: `loadstart`, then `progress`, `load` and `loadend`.
fn read(cx: &mut Cx<'_>, this: ObjectId, blob_id: ObjectId, how: ReadAs) -> Fallible<()> {
    let state = cx.page.with::<FileReaderObject, _>(this, |r| r.state)?;
    if state == LOADING {
        return Err(Exception::invalid_state(
            "The object is already busy reading Blobs.",
        ));
    }
    let (bytes, type_) = blob_contents(cx, blob_id)?;
    cx.page.with::<FileReaderObject, _>(this, |r| {
        r.state = LOADING;
        r.result = None;
        r.error = Value::Null;
    })?;
    let result = match how {
        ReadAs::ArrayBuffer => StringOrArrayBuffer::ArrayBuffer(bytes.to_vec()),
        ReadAs::BinaryString => {
            StringOrArrayBuffer::String(bytes.iter().map(|&b| b as char).collect())
        }
        ReadAs::Text(label) => StringOrArrayBuffer::String(decode_text(&bytes, label.as_deref())),
        ReadAs::DataUrl => {
            let mime = if type_.is_empty() {
                "application/octet-stream".to_string()
            } else {
                type_
            };
            StringOrArrayBuffer::String(format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&*bytes)
            ))
        }
    };
    let total = bytes.len();
    cx.pin(this);
    crate::event_loop::queue_task(cx.page, "file read", move |cx| {
        let still_loading = cx
            .page
            .try_with::<FileReaderObject, _>(this, |r| r.state == LOADING)
            .unwrap_or(false);
        if still_loading {
            fire_progress(cx, this, "loadstart", 0, total);
            let _ = cx.page.with::<FileReaderObject, _>(this, |r| {
                r.state = DONE;
                r.result = Some(result);
            });
            fire_progress(cx, this, "progress", total, total);
            fire_progress(cx, this, "load", total, total);
            fire_progress(cx, this, "loadend", total, total);
        }
        cx.unpin(this);
    });
    Ok(())
}

fn fire_progress(cx: &mut Cx<'_>, this: ObjectId, type_: &str, loaded: usize, total: usize) {
    let event = events::progress_event(cx, type_, loaded as f64, Some(total as f64));
    events::dispatch(cx, EventTargetRef::Object(this), event);
}

impl web::FileReaderImpl for Web {
    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(FileReaderObject {
            state: EMPTY,
            result: None,
            error: Value::Null,
        }))
    }

    fn read_as_array_buffer(cx: &mut Cx<'_>, this: ObjectId, blob: ObjectId) -> Fallible<()> {
        read(cx, this, blob, ReadAs::ArrayBuffer)
    }

    fn read_as_binary_string(cx: &mut Cx<'_>, this: ObjectId, blob: ObjectId) -> Fallible<()> {
        read(cx, this, blob, ReadAs::BinaryString)
    }

    fn read_as_text(
        cx: &mut Cx<'_>,
        this: ObjectId,
        blob: ObjectId,
        encoding: Option<String>,
    ) -> Fallible<()> {
        read(cx, this, blob, ReadAs::Text(encoding))
    }

    fn read_as_data_url(cx: &mut Cx<'_>, this: ObjectId, blob: ObjectId) -> Fallible<()> {
        read(cx, this, blob, ReadAs::DataUrl)
    }

    fn abort(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let was_loading = cx.page.with::<FileReaderObject, _>(this, |r| {
            let loading = r.state == LOADING;
            r.state = DONE;
            r.result = None;
            loading
        })?;
        if was_loading {
            let error = cx
                .script
                .exception_value(&Exception::abort("The read was aborted."));
            cx.page
                .with::<FileReaderObject, _>(this, |r| r.error = error)?;
            fire_progress(cx, this, "abort", 0, 0);
            fire_progress(cx, this, "loadend", 0, 0);
        }
        Ok(())
    }

    fn ready_state(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        cx.page.with::<FileReaderObject, _>(this, |r| r.state)
    }

    fn result(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<StringOrArrayBuffer>> {
        cx.page
            .with::<FileReaderObject, _>(this, |r| r.result.clone())
    }

    fn error(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        cx.page
            .with::<FileReaderObject, _>(this, |r| r.error.clone())
    }
}

// ---------------------------------------------------------------- FormData

#[derive(Clone)]
pub(crate) enum Entry {
    Text(String),
    /// A `File`, pinned while it is in the list.
    File(ObjectId),
}

pub struct FormDataObject {
    entries: Vec<(String, Entry)>,
}
platform_object!(FormDataObject, FormData);

fn form_data<R>(
    cx: &Cx<'_>,
    id: ObjectId,
    f: impl FnOnce(&mut FormDataObject) -> R,
) -> Fallible<R> {
    cx.page.with::<FormDataObject, _>(id, f)
}

fn entry_value(entry: &Entry) -> FileOrString {
    match entry {
        Entry::Text(t) => FileOrString::String(t.clone()),
        Entry::File(id) => FileOrString::File(*id),
    }
}

/// <https://xhr.spec.whatwg.org/#create-an-entry>: a blob becomes a file
/// named `blob`, or `filename` when given; a file keeps its name unless
/// `filename` renames it.
fn file_entry(cx: &mut Cx<'_>, blob_id: ObjectId, filename: Option<String>) -> Fallible<Entry> {
    let (is_file, name) = blob(cx, blob_id, |b| {
        (b.file.is_some(), b.file.as_ref().map(|f| f.name.clone()))
    })?;
    let id = match (is_file, filename) {
        (true, None) => blob_id,
        (_, filename) => {
            let (bytes, type_) = blob_contents(cx, blob_id)?;
            let last_modified = blob(cx, blob_id, |b| b.file.as_ref().map(|f| f.last_modified))?;
            new_file(
                cx,
                bytes.to_vec(),
                &type_,
                &filename.or(name).unwrap_or_else(|| "blob".to_string()),
                last_modified.unwrap_or_else(|| now_ms(cx)),
            )
        }
    };
    cx.pin(id);
    Ok(Entry::File(id))
}

fn unpin_entry(cx: &mut Cx<'_>, entry: &Entry) {
    if let Entry::File(id) = entry {
        cx.unpin(*id);
    }
}

/// <https://html.spec.whatwg.org/#constructing-the-form-data-set>, for
/// the controls there are: inputs, textareas and selects.
pub(crate) fn entry_list(
    cx: &mut Cx<'_>,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Fallible<Vec<(String, Entry)>> {
    let controls: Vec<NodeId> = {
        let dom = cx.dom();
        crate::forms::listed_controls(&dom, form)
            .into_iter()
            .filter(|&n| {
                dom.element(n)
                    .is_some_and(|el| matches!(&*el.name.local, "input" | "select" | "textarea"))
            })
            .collect()
    };
    let mut out = Vec::new();
    let disabled = |dom: &Dom, n: NodeId| {
        std::iter::once(n).chain(dom.ancestors(n)).any(|a| {
            dom.element(a).is_some_and(|el| {
                el.is_html()
                    && el.has_attr("disabled")
                    && matches!(
                        &*el.name.local,
                        "input" | "select" | "textarea" | "fieldset" | "button"
                    )
            })
        })
    };
    for control in controls {
        let (local, name, type_, is_disabled) = {
            let dom = cx.dom();
            let el = dom.element(control).expect("an element");
            (
                el.name.local.to_string(),
                el.attr("name").unwrap_or_default().to_string(),
                el.attr("type")
                    .unwrap_or("text")
                    .trim()
                    .to_ascii_lowercase(),
                disabled(&dom, control),
            )
        };
        if is_disabled || name.is_empty() {
            continue;
        }
        match local.as_str() {
            "input" => {
                match type_.as_str() {
                    "button" | "submit" | "reset" | "image" => continue,
                    "file" => {
                        // No file chosen sends an empty, nameless one.
                        let files = chosen_files(cx, control);
                        if files.is_empty() {
                            let now = now_ms(cx);
                            let id = new_file(cx, Vec::new(), "application/octet-stream", "", now);
                            cx.pin(id);
                            out.push((name.clone(), Entry::File(id)));
                        }
                        for file in files {
                            cx.pin(file);
                            out.push((name.clone(), Entry::File(file)));
                        }
                        continue;
                    }
                    "checkbox" | "radio" if !is_checked(cx, control) => continue,
                    _ => {}
                }
                if type_ == "hidden" && name == "_charset_" {
                    out.push((name, Entry::Text("UTF-8".to_string())));
                    continue;
                }
                let value = <Web as web::HTMLInputElementImpl>::value(cx, control)?;
                out.push((name, Entry::Text(value)));
            }
            "textarea" => {
                let value = <Web as web::HTMLTextAreaElementImpl>::value(cx, control)?;
                out.push((name, Entry::Text(value)));
            }
            "select" => {
                let selected = crate::forms::selected_options(cx, control);
                let dom = cx.dom();
                for option in selected {
                    if disabled(&dom, option) {
                        continue;
                    }
                    let value = crate::forms::option_value(&dom, option);
                    out.push((name.clone(), Entry::Text(value)));
                }
            }
            _ => {}
        }
    }
    if let Some(button) = submitter {
        let dom = cx.dom();
        if let Some(el) = dom.element(button)
            && let Some(name) = el.attr("name").filter(|n| !n.is_empty())
            && !disabled(&dom, button)
        {
            out.push((
                name.to_string(),
                Entry::Text(el.attr("value").unwrap_or_default().to_string()),
            ));
        }
    }
    Ok(out)
}

impl web::FormDataImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        form: Option<NodeId>,
        submitter: Option<NodeId>,
    ) -> Fallible<ObjectId> {
        let entries = match form {
            Some(form) => {
                node::check(cx, form)?;
                if !cx
                    .dom()
                    .element(form)
                    .is_some_and(|el| el.is_html() && &*el.name.local == "form")
                {
                    return Err(Exception::type_error("The argument is not a form element"));
                }
                if let Some(button) = submitter {
                    let owner_ok = cx.dom().ancestors(button).any(|a| a == form);
                    if !owner_ok {
                        return Err(Exception::not_found(
                            "The submitter is not owned by the form",
                        ));
                    }
                }
                entry_list(cx, form, submitter)?
            }
            None => Vec::new(),
        };
        Ok(cx.page.alloc(FormDataObject { entries }))
    }

    fn append(cx: &mut Cx<'_>, this: ObjectId, name: String, value: String) -> Fallible<()> {
        form_data(cx, this, |f| f.entries.push((name, Entry::Text(value))))
    }

    fn append_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        name: String,
        blob_value: ObjectId,
        filename: Option<String>,
    ) -> Fallible<()> {
        form_data(cx, this, |_| ())?;
        let entry = file_entry(cx, blob_value, filename)?;
        form_data(cx, this, |f| f.entries.push((name, entry)))
    }

    fn delete(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<()> {
        let removed = form_data(cx, this, |f| {
            let (gone, kept): (Vec<_>, Vec<_>) = f.entries.drain(..).partition(|(n, _)| *n == name);
            f.entries = kept;
            gone
        })?;
        for (_, entry) in &removed {
            unpin_entry(cx, entry);
        }
        Ok(())
    }

    fn get(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<Option<FileOrString>> {
        form_data(cx, this, |f| {
            f.entries
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, e)| entry_value(e))
        })
    }

    fn get_all(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<Vec<FileOrString>> {
        form_data(cx, this, |f| {
            f.entries
                .iter()
                .filter(|(n, _)| *n == name)
                .map(|(_, e)| entry_value(e))
                .collect()
        })
    }

    fn has(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<bool> {
        form_data(cx, this, |f| f.entries.iter().any(|(n, _)| *n == name))
    }

    fn set(cx: &mut Cx<'_>, this: ObjectId, name: String, value: String) -> Fallible<()> {
        set_entry(cx, this, name, Entry::Text(value))
    }

    fn set_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        name: String,
        blob_value: ObjectId,
        filename: Option<String>,
    ) -> Fallible<()> {
        form_data(cx, this, |_| ())?;
        let entry = file_entry(cx, blob_value, filename)?;
        set_entry(cx, this, name, entry)
    }

    fn iterate(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<(String, FileOrString)>> {
        form_data(cx, this, |f| {
            f.entries
                .iter()
                .map(|(n, e)| (n.clone(), entry_value(e)))
                .collect()
        })
    }
}

/// `set()`: the first entry with the name takes the value, the others go.
fn set_entry(cx: &mut Cx<'_>, this: ObjectId, name: String, entry: Entry) -> Fallible<()> {
    let removed = form_data(cx, this, |f| {
        let mut removed = Vec::new();
        let mut placed = false;
        let mut kept = Vec::with_capacity(f.entries.len());
        for (n, e) in f.entries.drain(..) {
            if n == name {
                if !placed {
                    kept.push((n, entry.clone()));
                    placed = true;
                }
                removed.push(e);
            } else {
                kept.push((n, e));
            }
        }
        if !placed {
            kept.push((name, entry));
        }
        f.entries = kept;
        removed
    })?;
    for e in &removed {
        unpin_entry(cx, e);
    }
    Ok(())
}

// ------------------------------------------------------ bodies and parsing

fn escape_name(name: &str) -> String {
    name.replace('\r', "%0D")
        .replace('\n', "%0A")
        .replace('"', "%22")
}

/// <https://html.spec.whatwg.org/#multipart/form-data-encoding-algorithm>:
/// the body a `FormData` is sent as, and its Content-Type.
/// A `FormData` holding `entries`.
pub(crate) fn form_data_object(page: &PageState, entries: Vec<(String, Entry)>) -> ObjectId {
    page.alloc(FormDataObject { entries })
}

/// The entries as `application/x-www-form-urlencoded`; a file stands for
/// its name.
pub(crate) fn urlencoded_body(cx: &Cx<'_>, this: ObjectId) -> Fallible<String> {
    let entries = form_data(cx, this, |f| f.entries.clone())?;
    let crlf = |text: &str| {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\n', "\r\n")
    };
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (name, entry) in &entries {
        let name = crlf(name);
        match entry {
            Entry::Text(value) => {
                out.append_pair(&name, &crlf(value));
            }
            Entry::File(id) => {
                let filename = blob(cx, *id, |b| {
                    b.file.as_ref().map(|f| f.name.clone()).unwrap_or_default()
                })?;
                out.append_pair(&name, &filename);
            }
        }
    }
    Ok(out.finish())
}

/// The entries as `text/plain`: `name=value` lines.
pub(crate) fn text_plain_body(cx: &Cx<'_>, this: ObjectId) -> Fallible<Vec<u8>> {
    let entries = form_data(cx, this, |f| f.entries.clone())?;
    let mut out = String::new();
    for (name, entry) in &entries {
        let value = match entry {
            Entry::Text(value) => value.clone(),
            Entry::File(id) => blob(cx, *id, |b| {
                b.file.as_ref().map(|f| f.name.clone()).unwrap_or_default()
            })?,
        };
        out.push_str(name);
        out.push('=');
        out.push_str(&value);
        out.push_str("\r\n");
    }
    Ok(out.into_bytes())
}

pub(crate) fn multipart_body(cx: &Cx<'_>, this: ObjectId) -> Fallible<(Vec<u8>, String)> {
    let entries = form_data(cx, this, |f| f.entries.clone())?;
    let boundary = format!(
        "----CatPawFormBoundary{}",
        crate::crypto::uuid_v4()?.replace('-', "")
    );
    let mut out = Vec::new();
    for (name, entry) in &entries {
        out.extend(format!("--{boundary}\r\n").into_bytes());
        match entry {
            Entry::Text(value) => {
                let value = value
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .replace('\n', "\r\n");
                out.extend(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"\r\n\r\n{value}\r\n",
                        escape_name(name)
                    )
                    .into_bytes(),
                );
            }
            Entry::File(id) => {
                let (bytes, type_) = blob_contents(cx, *id)?;
                let filename = blob(cx, *id, |b| {
                    b.file.as_ref().map(|f| f.name.clone()).unwrap_or_default()
                })?;
                let type_ = if type_.is_empty() {
                    "application/octet-stream".to_string()
                } else {
                    type_
                };
                out.extend(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {type_}\r\n\r\n",
                        escape_name(name),
                        escape_name(&filename)
                    )
                    .into_bytes(),
                );
                out.extend(bytes.iter());
                out.extend(b"\r\n");
            }
        }
    }
    out.extend(format!("--{boundary}--\r\n").into_bytes());
    Ok((out, format!("multipart/form-data; boundary={boundary}")))
}

/// A `FormData` from a body, by its Content-Type: urlencoded or multipart.
pub(crate) fn parse_body(
    cx: &mut Cx<'_>,
    bytes: &[u8],
    content_type: Option<&str>,
) -> Fallible<ObjectId> {
    let content_type = content_type.unwrap_or_default().trim();
    let (essence, params) = content_type.split_once(';').unwrap_or((content_type, ""));
    let essence = essence.trim().to_ascii_lowercase();
    let entries = if essence == "application/x-www-form-urlencoded" {
        url::form_urlencoded::parse(bytes)
            .into_owned()
            .map(|(n, v)| (n, Entry::Text(v)))
            .collect()
    } else if essence == "multipart/form-data" {
        let boundary = params
            .split(';')
            .filter_map(|p| p.trim().split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("boundary"))
            .map(|(_, v)| v.trim().trim_matches('"').to_string())
            .ok_or_else(|| Exception::type_error("The multipart body has no boundary"))?;
        parse_multipart(cx, bytes, &boundary)?
    } else {
        return Err(Exception::type_error(
            "Could not parse content as FormData.",
        ));
    };
    Ok(cx.page.alloc(FormDataObject { entries }))
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn parse_multipart(
    cx: &mut Cx<'_>,
    bytes: &[u8],
    boundary: &str,
) -> Fallible<Vec<(String, Entry)>> {
    let delimiter = format!("--{boundary}").into_bytes();
    let mut entries = Vec::new();
    let mut pos = find(bytes, &delimiter, 0)
        .ok_or_else(|| Exception::type_error("Could not parse content as FormData."))?
        + delimiter.len();
    loop {
        if bytes[pos..].starts_with(b"--") {
            break;
        }
        // Past the CRLF after the delimiter.
        pos = find(bytes, b"\r\n", pos)
            .map(|i| i + 2)
            .unwrap_or(bytes.len());
        let headers_end = find(bytes, b"\r\n\r\n", pos)
            .ok_or_else(|| Exception::type_error("Could not parse content as FormData."))?;
        let headers = String::from_utf8_lossy(&bytes[pos..headers_end]).into_owned();
        let body_start = headers_end + 4;
        let next = find(bytes, &delimiter, body_start)
            .ok_or_else(|| Exception::type_error("Could not parse content as FormData."))?;
        let body_end = next.saturating_sub(2).max(body_start);
        let body = &bytes[body_start..body_end];
        let mut name = None;
        let mut filename = None;
        let mut type_ = String::new();
        for line in headers.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "content-disposition" => {
                    for param in value.split(';').skip(1) {
                        if let Some((k, v)) = param.trim().split_once('=') {
                            let v = v
                                .trim()
                                .trim_matches('"')
                                .replace("%22", "\"")
                                .replace("%0D", "\r")
                                .replace("%0A", "\n");
                            match k.trim() {
                                "name" => name = Some(v),
                                "filename" => filename = Some(v),
                                _ => {}
                            }
                        }
                    }
                }
                "content-type" => type_ = value.trim().to_string(),
                _ => {}
            }
        }
        let Some(name) = name else {
            return Err(Exception::type_error(
                "Could not parse content as FormData.",
            ));
        };
        let entry = match filename {
            Some(filename) => {
                let id = new_file(cx, body.to_vec(), &type_, &filename, now_ms(cx));
                cx.pin(id);
                Entry::File(id)
            }
            None => Entry::Text(String::from_utf8_lossy(body).into_owned()),
        };
        entries.push((name, entry));
        pos = next + delimiter.len();
    }
    Ok(entries)
}
