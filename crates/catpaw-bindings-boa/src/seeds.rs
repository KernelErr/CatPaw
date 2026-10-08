//! The seeds of a realm's random sequences in a seeded run.
//!
//! Every realm (a document, a frame's document, a worker) gets sequences
//! of its own, derived from the session's seed, the document's URL and
//! what tells the realm from others with the same URL: its tab, its frame
//! and how many documents of that URL the frame loaded before. A worker's
//! derive from the seed of the realm that started it (which the embedder
//! makes the worker's run seed), its script's URL, and the worker it is
//! there. A run asks for the same realms in the same order, so it gets the
//! same numbers wherever it runs; two tabs or frames showing one page, a
//! page loaded again, and their workers, do not repeat each other's.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use catpaw_web::PageState;
use catpaw_web::frames::FrameTree;

use crate::rt::Realm;

/// Documents loaded so far per frame tree on this thread: by frame and
/// URL. A tree's counts go with it.
type Loads = Vec<(Weak<RefCell<FrameTree>>, HashMap<(u32, String), u64>)>;

thread_local! {
    static LOADS: RefCell<Loads> = const { RefCell::new(Vec::new()) };
}

/// What workers' seeds are mixed with first, so that they never meet a
/// frame's.
const WORKER: u64 = 0x776f_726b_6572;

/// What the documents of a tab other than the first are mixed with first.
const TAB: u64 = 0x0074_6162;

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn mix(seed: u64, value: u64) -> u64 {
    splitmix(seed ^ splitmix(value))
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// How many documents of `url` frame `frame` of `tree` loaded before this
/// one (which it counts).
fn previous_loads(tree: &Rc<RefCell<FrameTree>>, frame: u32, url: &str) -> u64 {
    LOADS.with(|loads| {
        let mut loads = loads.borrow_mut();
        loads.retain(|(tree, _)| tree.strong_count() > 0);
        let at = match loads
            .iter()
            .position(|(known, _)| std::ptr::eq(known.as_ptr(), Rc::as_ptr(tree)))
        {
            Some(at) => at,
            None => {
                loads.push((Rc::downgrade(tree), HashMap::new()));
                loads.len() - 1
            }
        };
        let count = loads[at].1.entry((frame, url.to_string())).or_insert(0);
        *count += 1;
        *count - 1
    })
}

/// The seed of the realm being made for `page`, when the run is seeded.
/// The first document of the first tab's top frame gets the seed it always
/// had: the session's mixed with the URL.
pub(crate) fn realm_seed(page: &PageState, realm: Realm) -> Option<u64> {
    let session = page.config.random_seed?;
    let url = page.url.borrow().to_string();
    let mut seed = session;
    for b in url.bytes() {
        seed = (seed ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    match realm {
        Realm::Window => {
            let tab = page.config.tab;
            if tab != 0 {
                seed = mix(mix(seed, TAB), u64::from(tab));
            }
            let frame = page.frames.id().map_or(0, |frame| frame.0);
            let before = previous_loads(&page.frames.tree(), frame, &url);
            if frame != 0 || before != 0 {
                seed = mix(mix(seed, u64::from(frame)), before);
            }
        }
        // The run seed here is the seed of the realm that started the
        // worker, which tells its tab, frame and load apart.
        Realm::Worker => {
            let (id, name) = page
                .workers
                .role()
                .map(|role| (role.id.0, role.name))
                .unwrap_or_default();
            seed = mix(mix(mix(seed, WORKER), u64::from(id)), fnv(name.as_bytes()));
        }
    }
    Some(seed)
}
