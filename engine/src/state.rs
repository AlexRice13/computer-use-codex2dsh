use crate::policy::Policy;
use std::collections::{HashMap, HashSet};

/// Geometry of one screenshot handed out with a snapshot: which part of the
/// window it covers and how many JPEG px it has per logical window px.
///
/// Coordinate tools receive model-measured pixels from this image, so they
/// must undo both the crop offset and the downscale before reaching the
/// screen. A default snapshot is `offset (0,0)`, `scale 1.0`, which reproduces
/// the original helper's window-relative logical coordinates exactly.
#[derive(Clone)]
pub struct ShotGeom {
    pub id: String,
    pub offset: (f64, f64),
    pub scale: f64,
}

/// A get_window_state snapshot kept per window so element_index actions
/// resolve against the tree the model just saw (the helper owns this cache;
/// a newer snapshot for the same window replaces it wholesale).
#[allow(dead_code)] // metadata kept for diagnostics / future invalidation
pub struct WindowSnapshot {
    pub hwnd: isize,
    pub taken_ms: u64,
    /// Window origin when captured; tree rects are screen-space.
    pub origin: (i32, i32),
    /// Window size when captured (cheap snapshot invalidation check).
    pub size: (i32, i32),
    pub elements: Vec<crate::win::uia::ElementRecord>,
    /// Ids of screenshots handed out with this snapshot.
    pub shot_ids: Vec<String>,
    /// Per-screenshot crop/scale geometry (coordinate mapping).
    pub shots: Vec<ShotGeom>,
    /// Rendered tree text (replayed on cache hits, revision stays put).
    pub tree: String,
    pub focused_element: Option<String>,
    pub document_text: Option<String>,
    /// Window title when captured (cheap invalidation check).
    pub title: String,
    /// Value of `EngineState::last_input_seq` when the tree was captured.
    /// A snapshot is only replayable while it still equals the counter: any
    /// input since then makes the rendered tree stale, so the next
    /// `get_window_state` must re-walk (the original always re-renders after
    /// input). Element_index actions keep using it regardless, which is what
    /// the documented contract asks for — a delivered click must not throw
    /// away the indexes the model is still working from.
    pub at_seq: u64,
}

/// Process-wide helper state. The RPC channel is serialized, so this lives
/// behind a single owner in serve().
pub struct EngineState {
    #[allow(dead_code)] // stub policy allows everything today; kept for parity
    pub policy: Policy,
    /// Session-scoped approved app identities (per-turn cleanup per
    /// `end_turn`, matching the observed helper).
    pub approved_apps: HashSet<String>,
    /// Last turn key (`<session_id>\0<turn_id>`) seen on the wire.
    pub last_turn_key: Option<String>,
    /// Latest snapshot per window (element_index resolution source).
    pub snapshots: HashMap<isize, WindowSnapshot>,
    /// Latest screenshot geometry per window, independent of the tree cache:
    /// `include_text: false` captures still have to map their pixels back to
    /// the screen for coordinate input.
    pub shot_geoms: HashMap<isize, Vec<ShotGeom>>,
    /// Monotonic counter bumped on every successful accessibility snapshot
    /// (diagnostic_state: snapshotRevision/accessibilityRevision).
    pub snapshot_revision: u64,
    /// Number of cached snapshot artifacts (original: 3 per captured tree).
    pub snapshot_count: u64,
    /// Reason the last capture attempt failed, cleared on success.
    pub last_capture_invalidation: Option<String>,
    /// Identity of the last window a snapshot was taken of
    /// (diagnostic_state: lastWindowIdentity).
    pub last_window_identity: Option<serde_json::Value>,
    /// Set when a browser window tripped the URL policy gate; the rest of
    /// the turn is stopped until `end_turn`.
    pub browser_policy_stopped: bool,
    /// Bumped by every window-touching action. Snapshots captured at an older
    /// value are never replayed as a tree (see `WindowSnapshot::at_seq`).
    pub last_input_seq: u64,
}

impl EngineState {
    pub fn new(policy: Policy) -> Self {
        EngineState {
            policy,
            approved_apps: HashSet::new(),
            last_turn_key: None,
            snapshots: HashMap::new(),
            shot_geoms: HashMap::new(),
            snapshot_revision: 0,
            snapshot_count: 0,
            last_capture_invalidation: None,
            last_window_identity: None,
            browser_policy_stopped: false,
            last_input_seq: 0,
        }
    }
}

/// Response shapes accepted by the client (spec §A.4).
pub enum Response {
    Result(serde_json::Value),
    Error(String),
    Approval {
        app: String,
        display_name: String,
        risk_level: Option<String>,
        allow_persistent_approval: bool,
    },
}
