use crate::policy::Policy;
use std::collections::{HashMap, HashSet};

/// 跟 snapshot 一起发出去的一份截图的几何参数：盖住窗口的哪一块，
/// 以及每个逻辑窗口像素对应几个 JPEG 像素。
///
/// 坐标工具收到的像素是模型从这张图上量出来的，所以落到屏幕之前，裁剪偏移
/// 和缩放都得反算回去。默认 snapshot 是 `offset (0,0)`、`scale 1.0`，这时
/// 算出来的坐标跟原版 helper 的窗口相对逻辑坐标完全一致。
#[derive(Clone)]
pub struct ShotGeom {
    pub id: String,
    pub offset: (f64, f64),
    pub scale: f64,
}

/// 每个窗口各留一份 get_window_state 的 snapshot，这样 element_index 类动作
/// 能落在模型刚看过的那棵树上（缓存归 helper 自己管；同一个窗口来了新
/// snapshot 就整份换掉）。
#[allow(dead_code)] // 这些字段留着做诊断，以后也可能拿来判失效
pub struct WindowSnapshot {
    pub hwnd: isize,
    pub taken_ms: u64,
    /// 抓的时候窗口的原点；树里的坐标是屏幕坐标。
    pub origin: (i32, i32),
    /// 抓的时候窗口多大（判断 snapshot 失没失效，这样最省事）。
    pub size: (i32, i32),
    pub elements: Vec<crate::win::uia::ElementRecord>,
    /// 跟这个 snapshot 一起发出去的截图 id。
    pub shot_ids: Vec<String>,
    /// 每张截图的裁剪/缩放参数（算坐标用）。
    pub shots: Vec<ShotGeom>,
    /// 渲染好的树文本（命中缓存就原样重复，revision 不动）。
    pub tree: String,
    pub focused_element: Option<String>,
    pub document_text: Option<String>,
    /// 抓的时候窗口标题（判断失没失效，这样最省事）。
    pub title: String,
    /// 抓这棵树时 `EngineState::last_input_seq` 的值。
    /// 只有它还等于计数器时 snapshot 才能原样重放：之后只要有过输入，渲染好
    /// 的树就旧了，下一次 `get_window_state` 必须重新走一遍（原版每次输入之后
    /// 都重新渲染）。但 element_index 动作照用不误，文档里的契约就是这么要求
    /// 的——已经点下去的那一下，不能把模型还在用的那些 index 作废。
    pub at_seq: u64,
}

/// 整个进程共用的 helper 状态。RPC 是串行的，所以在 serve() 里只有它一个
/// 持有者。
pub struct EngineState {
    #[allow(dead_code)] // 桩策略现在全放行，留着是为了跟原版对齐
    pub policy: Policy,
    /// 会话级的已批准 app 身份（每到 `end_turn` 清一遍，跟实测的原版一样）。
    pub approved_apps: HashSet<String>,
    /// 通信里见过的最后一个 turn key（`<session_id>\0<turn_id>`）。
    pub last_turn_key: Option<String>,
    /// 每个窗口最新的 snapshot（解析 element_index 就靠它）。
    pub snapshots: HashMap<isize, WindowSnapshot>,
    /// 每个窗口最新的截图几何，跟树缓存分开存：`include_text: false` 抓的图
    /// 一样得把像素换算回屏幕坐标，点击才点得准。
    pub shot_geoms: HashMap<isize, Vec<ShotGeom>>,
    /// 只增不减的计数器，每次成功抓无障碍树就加一
    /// （diagnostic_state 里的 snapshotRevision/accessibilityRevision）。
    pub snapshot_revision: u64,
    /// 缓存下来的 snapshot 产物个数（原版每抓一棵树算 3 个）。
    pub snapshot_count: u64,
    /// 上次抓失败的原因，成功了就清掉。
    pub last_capture_invalidation: Option<String>,
    /// 最后抓 snapshot 的那个窗口的身份
    /// （diagnostic_state 里的 lastWindowIdentity）。
    pub last_window_identity: Option<serde_json::Value>,
    /// 浏览器窗口撞上 URL 策略闸门时置位；这一轮剩下的动作全停，直到
    /// `end_turn`。
    pub browser_policy_stopped: bool,
    /// 每个碰窗口的动作都会把它加一。在更旧的值上抓的 snapshot 不会再被当成
    /// 树重放（见 `WindowSnapshot::at_seq`）。
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

/// 客户端认的几种应答形状（规范 §A.4）。
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
