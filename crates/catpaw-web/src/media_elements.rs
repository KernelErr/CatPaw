//! `<video>` and `<audio>`: elements that play nothing.
//!
//! There is no decoder, so a media element never has data: it stays paused
//! at its start, `canPlayType` answers "" for every type, and `play()`
//! rejects with `NotSupportedError`, as a browser does for a source it
//! cannot play. What scripts set (the time, volume, rate, muting, the text
//! tracks they add) is kept and read back.

use catpaw_dom::NodeId;
use catpaw_js::{Exception, Fallible, ObjectId, PromiseRef};

use crate::generated::{self as web, CanPlayTypeResult, TextTrackKind, TextTrackMode};
use crate::page::Cx;
use crate::{Web, node, platform_object};

/// What scripts set on a media element.
pub(crate) struct MediaState {
    current_time: f64,
    volume: f64,
    muted: bool,
    playback_rate: f64,
    default_playback_rate: f64,
    /// The element's `TextTrackList`, pinned, once asked for.
    text_tracks: Option<ObjectId>,
}

pub struct TimeRangesObject;
platform_object!(TimeRangesObject, TimeRanges);

pub struct TextTrackListObject {
    tracks: Vec<ObjectId>,
}
platform_object!(
    TextTrackListObject,
    TextTrackList,
    pinned = |l| l.tracks.clone()
);

pub struct TextTrackObject {
    kind: TextTrackKind,
    label: String,
    language: String,
    mode: TextTrackMode,
}
platform_object!(TextTrackObject, TextTrack);

/// The element's state, made from its content attributes the first time.
fn with_state<R>(cx: &mut Cx<'_>, this: NodeId, f: impl FnOnce(&mut MediaState) -> R) -> R {
    let muted = cx.dom().attr(this, "muted").is_some();
    let mut media = cx.page.media.borrow_mut();
    let state = media.entry(this).or_insert_with(|| MediaState {
        current_time: 0.0,
        volume: 1.0,
        muted,
        playback_rate: 1.0,
        default_playback_rate: 1.0,
        text_tracks: None,
    });
    f(state)
}

fn finite(value: f64) -> Fallible<f64> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(Exception::type_error("The value is not a finite number"))
    }
}

impl web::HTMLMediaElementImpl for Web {
    fn error(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<ObjectId>> {
        node::check(cx, this)?;
        Ok(None)
    }

    fn current_src(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(String::new())
    }

    fn network_state(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u16> {
        node::check(cx, this)?;
        Ok(0)
    }

    fn buffered(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        node::check(cx, this)?;
        Ok(cx.page.alloc(TimeRangesObject))
    }

    fn load(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        node::check(cx, this)?;
        with_state(cx, this, |s| s.current_time = 0.0);
        Ok(())
    }

    fn can_play_type(cx: &mut Cx<'_>, this: NodeId, _type: String) -> Fallible<CanPlayTypeResult> {
        node::check(cx, this)?;
        Ok(CanPlayTypeResult::Empty)
    }

    fn ready_state(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u16> {
        node::check(cx, this)?;
        Ok(0)
    }

    fn seeking(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        node::check(cx, this)?;
        Ok(false)
    }

    fn current_time(cx: &mut Cx<'_>, this: NodeId) -> Fallible<f64> {
        node::check(cx, this)?;
        Ok(with_state(cx, this, |s| s.current_time))
    }

    fn set_current_time(cx: &mut Cx<'_>, this: NodeId, value: f64) -> Fallible<()> {
        node::check(cx, this)?;
        let value = finite(value)?;
        with_state(cx, this, |s| s.current_time = value);
        Ok(())
    }

    fn duration(cx: &mut Cx<'_>, this: NodeId) -> Fallible<f64> {
        node::check(cx, this)?;
        Ok(f64::NAN)
    }

    fn paused(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        node::check(cx, this)?;
        Ok(true)
    }

    fn default_playback_rate(cx: &mut Cx<'_>, this: NodeId) -> Fallible<f64> {
        node::check(cx, this)?;
        Ok(with_state(cx, this, |s| s.default_playback_rate))
    }

    fn set_default_playback_rate(cx: &mut Cx<'_>, this: NodeId, value: f64) -> Fallible<()> {
        node::check(cx, this)?;
        let value = finite(value)?;
        with_state(cx, this, |s| s.default_playback_rate = value);
        Ok(())
    }

    fn playback_rate(cx: &mut Cx<'_>, this: NodeId) -> Fallible<f64> {
        node::check(cx, this)?;
        Ok(with_state(cx, this, |s| s.playback_rate))
    }

    fn set_playback_rate(cx: &mut Cx<'_>, this: NodeId, value: f64) -> Fallible<()> {
        node::check(cx, this)?;
        let value = finite(value)?;
        with_state(cx, this, |s| s.playback_rate = value);
        Ok(())
    }

    fn played(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        node::check(cx, this)?;
        Ok(cx.page.alloc(TimeRangesObject))
    }

    fn seekable(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        node::check(cx, this)?;
        Ok(cx.page.alloc(TimeRangesObject))
    }

    fn ended(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        node::check(cx, this)?;
        Ok(false)
    }

    fn play(cx: &mut Cx<'_>, this: NodeId) -> Fallible<PromiseRef> {
        node::check(cx, this)?;
        let promise = cx.script.new_promise();
        cx.script.reject_promise(
            &promise,
            Exception::dom("NotSupportedError", "The element has no supported sources."),
        );
        Ok(promise)
    }

    fn pause(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        node::check(cx, this)?;
        Ok(())
    }

    fn volume(cx: &mut Cx<'_>, this: NodeId) -> Fallible<f64> {
        node::check(cx, this)?;
        Ok(with_state(cx, this, |s| s.volume))
    }

    fn set_volume(cx: &mut Cx<'_>, this: NodeId, value: f64) -> Fallible<()> {
        node::check(cx, this)?;
        if !(0.0..=1.0).contains(&value) {
            return Err(Exception::dom(
                "IndexSizeError",
                format!("The volume provided ({value}) is outside the range [0, 1]."),
            ));
        }
        with_state(cx, this, |s| s.volume = value);
        Ok(())
    }

    fn muted(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        node::check(cx, this)?;
        Ok(with_state(cx, this, |s| s.muted))
    }

    fn set_muted(cx: &mut Cx<'_>, this: NodeId, value: bool) -> Fallible<()> {
        node::check(cx, this)?;
        with_state(cx, this, |s| s.muted = value);
        Ok(())
    }

    fn text_tracks(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        node::check(cx, this)?;
        Ok(text_track_list(cx, this))
    }

    fn add_text_track(
        cx: &mut Cx<'_>,
        this: NodeId,
        kind: TextTrackKind,
        label: String,
        language: String,
    ) -> Fallible<ObjectId> {
        node::check(cx, this)?;
        let list = text_track_list(cx, this);
        let track = cx.page.alloc(TextTrackObject {
            kind,
            label,
            language,
            mode: TextTrackMode::Hidden,
        });
        cx.pin(track);
        cx.page
            .with::<TextTrackListObject, _>(list, |l| l.tracks.push(track))?;
        Ok(track)
    }
}

/// The element's `TextTrackList`: the same object every time.
fn text_track_list(cx: &mut Cx<'_>, this: NodeId) -> ObjectId {
    if let Some(list) = with_state(cx, this, |s| s.text_tracks) {
        return list;
    }
    let list = cx.page.alloc(TextTrackListObject { tracks: Vec::new() });
    cx.pin(list);
    with_state(cx, this, |s| s.text_tracks = Some(list));
    list
}

impl web::HTMLVideoElementImpl for Web {
    fn video_width(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u32> {
        node::check(cx, this)?;
        Ok(0)
    }

    fn video_height(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u32> {
        node::check(cx, this)?;
        Ok(0)
    }
}

impl web::TimeRangesImpl for Web {
    fn length(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<u32> {
        Ok(0)
    }

    fn start(_cx: &mut Cx<'_>, _this: ObjectId, index: u32) -> Fallible<f64> {
        Err(out_of_range(index))
    }

    fn end(_cx: &mut Cx<'_>, _this: ObjectId, index: u32) -> Fallible<f64> {
        Err(out_of_range(index))
    }
}

fn out_of_range(index: u32) -> Exception {
    Exception::dom(
        "IndexSizeError",
        format!("The index provided ({index}) is greater than the maximum bound (0)."),
    )
}

impl web::TextTrackListImpl for Web {
    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        cx.page
            .with::<TextTrackListObject, _>(this, |l| l.tracks.len() as u32)
    }

    /// Tracks made by `addTextTrack` (the only ones there are) have no id.
    fn get_track_by_id(cx: &mut Cx<'_>, this: ObjectId, _id: String) -> Fallible<Option<ObjectId>> {
        cx.page.with::<TextTrackListObject, _>(this, |_| None)
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<ObjectId>> {
        cx.page
            .with::<TextTrackListObject, _>(this, |l| l.tracks.get(index as usize).copied())
    }
}

impl web::TextTrackImpl for Web {
    fn kind(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<TextTrackKind> {
        cx.page.with::<TextTrackObject, _>(this, |t| t.kind)
    }

    fn label(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        cx.page
            .with::<TextTrackObject, _>(this, |t| t.label.clone())
    }

    fn language(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        cx.page
            .with::<TextTrackObject, _>(this, |t| t.language.clone())
    }

    fn id(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        cx.page.with::<TextTrackObject, _>(this, |_| String::new())
    }

    fn mode(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<TextTrackMode> {
        cx.page.with::<TextTrackObject, _>(this, |t| t.mode)
    }

    fn set_mode(cx: &mut Cx<'_>, this: ObjectId, value: TextTrackMode) -> Fallible<()> {
        cx.page.with::<TextTrackObject, _>(this, |t| t.mode = value)
    }
}
