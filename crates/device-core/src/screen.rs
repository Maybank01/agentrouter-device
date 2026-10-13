//! Looking at and operating the screen (LINKED-DEVICES.md §20; CAPABILITIES-1029 A2–A5): on-demand
//! screenshots (no desktop streaming), the window list, mouse and keyboard actions whose coordinates
//! belong to a screenshot, GUI programs started and closed by name, and short screen recordings as GIF.
//!
//! Owner rules (2026-10-10): approvals are a false need. The person turns the screen on once on this
//! computer (`agentrouter screen on` / `link --screen`); after that nothing is asked. What is never done:
//! input while the secure desktop (UAC, lock screen) is up, typing into a password box, acting on a window
//! the person marked private (those are blacked out in every picture), and grabbing the mouse while the
//! person is using the computer. A bar at the top of the screen says the AI is operating it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Access;
use crate::gate::Scope;
use crate::protocol::DeviceError;
use crate::util::{b64, clip, now_ms};

#[cfg(windows)]
mod banner;
pub mod image;
#[cfg(windows)]
mod win;

/// The screen actions (in `info.actions` where the platform has them).
pub const ACTIONS: &[&str] = &["screenshot", "windows", "gui", "app", "screen_record"];

/// A screenshot's coordinates stop being valid after this (LINKED-DEVICES.md §20.1).
const SHOT_TTL_MS: i64 = 60_000;
const SHOTS_KEPT: usize = 30;
const MAX_EDGE_DEFAULT: usize = 1280;
const MAX_EDGE_MAX: usize = 1568;
const RECORD_EDGE_DEFAULT: usize = 800;
/// The person used mouse or keyboard this recently: the AI waits (§20.3).
const PERSON_ACTIVE_MS: u32 = 3_000;
/// What fits in one device frame (8 MiB) after base64.
const IMAGE_BYTES_MAX: usize = 5_500_000;
const ACTIONS_MAX: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl Rect {
    pub fn contains(&self, x: i64, y: i64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.width).min(o.x + o.width);
        let y1 = (self.y + self.height).min(o.y + o.height);
        (x1 > x0 && y1 > y0).then_some(Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Display {
    pub index: usize,
    pub rect: Rect,
    pub primary: bool,
}

#[derive(Debug, Clone)]
pub struct Window {
    pub id: u64,
    pub title: String,
    pub app: String,
    pub pid: u32,
    pub rect: Rect,
    pub minimized: bool,
    pub focused: bool,
}

/// What a screenshot covered and how it was scaled: actions name it to place their coordinates.
#[derive(Debug, Clone)]
struct Shot {
    id: String,
    /// The captured rectangle, physical pixels of the virtual screen.
    rect: Rect,
    width: usize,
    height: usize,
    at: i64,
    layout: String,
    /// How to take the same picture again (`screenshot_after`).
    target: Target,
}

#[derive(Debug, Clone)]
enum Target {
    Display(usize),
    Window(u64),
    Region(Rect),
}

#[derive(Debug, Clone)]
struct Launched {
    pid: Option<u32>,
    name: String,
}

#[derive(Default)]
pub struct Screen {
    enabled: AtomicBool,
    private: RwLock<Vec<String>>,
    shots: Mutex<HashMap<String, Vec<Shot>>>,
    launched: Mutex<HashMap<String, Vec<Launched>>>,
    next: AtomicU64,
}

fn error(code: &'static str, message: impl Into<String>) -> DeviceError {
    DeviceError::new(code, message)
}

/// `next` when the person has not turned the screen on here.
pub const NEXT_SCREEN_OFF: &str = "这台电脑没有打开屏幕功能。请用户在这台电脑上运行 agentrouter screen on（或 agentrouter link --screen）后再试。";

fn int(args: &Value, key: &str) -> Option<i64> {
    args.get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64)))
}

fn same_app(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim().to_lowercase();
        s.strip_suffix(".exe").map(str::to_string).unwrap_or(s)
    };
    !a.trim().is_empty() && norm(a) == norm(b)
}

/// Window ids travel as `w<number>` (the model never sees raw handles as numbers it might do maths on).
pub fn window_ref(id: u64) -> String {
    format!("w{id}")
}

fn parse_window(value: Option<&Value>) -> Option<u64> {
    let v = value?;
    v.as_u64().or_else(|| {
        v.as_str()
            .and_then(|s| s.trim().trim_start_matches('w').parse().ok())
    })
}

impl Screen {
    pub fn set(&self, on: bool, private: &[String]) {
        self.enabled.store(on && supported(), Ordering::SeqCst);
        *self.private.write().unwrap() = private.to_vec();
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// The person's 「不给 AI 看」 programs.
    fn is_private(&self, app: &str) -> bool {
        self.private
            .read()
            .unwrap()
            .iter()
            .any(|p| same_app(p, app))
    }

    fn check_on(&self) -> Result<(), DeviceError> {
        if !supported() {
            return Err(error(
                "UNSUPPORTED",
                "这台电脑的系统还不支持看屏幕和操作（目前只有 Windows）",
            ));
        }
        if !self.enabled() {
            return Err(error("SCREEN_OFF", "这台电脑没有打开屏幕功能").next(NEXT_SCREEN_OFF));
        }
        Ok(())
    }

    fn check_input(&self, scope: &Scope) -> Result<(), DeviceError> {
        match scope.access {
            Access::Readonly => Err(DeviceError::denied(
                "这台设备设成了只读：可以截图，不能操作鼠标键盘或打开程序",
            )
            .next(crate::protocol::NEXT_READONLY)),
            Access::Confirm => Err(DeviceError::denied(
                "这台设备设成了每条都确认：不能让 AI 操作鼠标键盘或打开程序",
            )
            .next("请用户在这台电脑上运行 agentrouter access folders 后再试。")),
            Access::Folders | Access::Full => Ok(()),
        }
    }

    fn remember(&self, session: &str, shot: Shot) {
        let mut shots = self.shots.lock().unwrap();
        let list = shots.entry(session.to_string()).or_default();
        list.push(shot);
        if list.len() > SHOTS_KEPT {
            list.remove(0);
        }
    }

    fn shot(&self, session: &str, id: Option<&str>) -> Result<Shot, DeviceError> {
        let shots = self.shots.lock().unwrap();
        let list = shots.get(session);
        let found = match id.filter(|s| !s.is_empty()) {
            Some(id) => list.and_then(|l| l.iter().find(|s| s.id == id)).cloned(),
            None => list.and_then(|l| l.last()).cloned(),
        };
        let Some(shot) = found else {
            return Err(
                error("STALE_SCREENSHOT", "没有这张截图（或这个对话还没截过图）")
                    .next("先用 screenshot 截一张，再按那张图的坐标操作。"),
            );
        };
        if now_ms() - shot.at > SHOT_TTL_MS || shot.layout != platform::layout() {
            return Err(
                error("STALE_SCREENSHOT", "这张截图已经过期，或屏幕布局变了")
                    .next("重新 screenshot 截一张，按新图的坐标操作。"),
            );
        }
        Ok(shot)
    }

    fn private_windows(&self) -> Vec<Window> {
        platform::windows()
            .into_iter()
            .filter(|w| !w.minimized && self.is_private(&w.app))
            .collect()
    }

    /// Capture a target, black out private windows, scale it.
    fn grab(
        &self,
        target: &Target,
        max_edge: usize,
    ) -> Result<(Rect, image::Rgb, Option<usize>), DeviceError> {
        let displays = platform::displays();
        let (rect, display) = match target {
            Target::Display(i) => {
                let d = displays.get(*i).ok_or_else(|| {
                    error(
                        "NOT_FOUND",
                        format!("没有第 {i} 块屏幕（共 {} 块）", displays.len()),
                    )
                })?;
                (d.rect, Some(d.index))
            }
            Target::Window(id) => {
                if !platform::is_window(*id) {
                    return Err(
                        error("NOT_FOUND", "这个窗口已经关了").next("用 windows 重新列出窗口。")
                    );
                }
                let w = platform::windows().into_iter().find(|w| w.id == *id);
                if let Some(w) = &w
                    && self.is_private(&w.app)
                {
                    return Err(DeviceError::denied("用户把这个程序设成了不给 AI 看"));
                }
                platform::focus(*id);
                std::thread::sleep(Duration::from_millis(200));
                let bounds = platform::window_bounds(*id)
                    .ok_or_else(|| error("NOT_FOUND", "这个窗口已经关了"))?;
                let r = bounds
                    .intersect(&platform::virtual_screen())
                    .ok_or_else(|| error("FAILED", "这个窗口不在屏幕上"))?;
                (r, None)
            }
            Target::Region(r) => (
                r.intersect(&platform::virtual_screen())
                    .ok_or_else(|| error("FAILED", "这块区域不在屏幕上"))?,
                None,
            ),
        };
        if !platform::input_desktop_available() {
            return Err(secure_desktop());
        }
        let mut frame = platform::capture(rect).map_err(|e| error("FAILED", e))?;
        for w in self.private_windows() {
            if let Some(hit) = w.rect.intersect(&rect) {
                frame.black_out(hit.x - rect.x, hit.y - rect.y, hit.width, hit.height);
            }
        }
        let (w, h) = image::fit(frame.width, frame.height, max_edge);
        Ok((rect, frame.scaled(w, h), display))
    }

    fn target_of(&self, session: &str, args: &Value) -> Result<Target, DeviceError> {
        match args
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("screen")
        {
            "window" => parse_window(args.get("window"))
                .map(Target::Window)
                .ok_or_else(|| error("FAILED", "target window 要写 window（windows 列出的编号）")),
            "region" => {
                let r = args.get("region").cloned().unwrap_or(Value::Null);
                let (Some(x), Some(y), Some(w), Some(h)) = (
                    int(&r, "x"),
                    int(&r, "y"),
                    int(&r, "width"),
                    int(&r, "height"),
                ) else {
                    return Err(error("FAILED", "region 要写 {x, y, width, height}"));
                };
                let shot = self.shot(session, args.get("shot").and_then(Value::as_str))?;
                let (px, py) = to_physical(&shot, x, y);
                let (qx, qy) = to_physical(&shot, x + w.max(1), y + h.max(1));
                Ok(Target::Region(Rect {
                    x: px,
                    y: py,
                    width: (qx - px).max(1),
                    height: (qy - py).max(1),
                }))
            }
            _ => {
                let displays = platform::displays();
                let i = int(args, "display")
                    .map(|d| d.max(0) as usize)
                    .unwrap_or_else(|| displays.iter().position(|d| d.primary).unwrap_or(0));
                Ok(Target::Display(i))
            }
        }
    }

    fn take(
        &self,
        session: &str,
        target: Target,
        max_edge: usize,
        grid: bool,
        quality: u8,
    ) -> Result<Value, DeviceError> {
        let (rect, mut rgb, display) = self.grab(&target, max_edge)?;
        if grid {
            rgb.grid(100);
        }
        let bytes = rgb.jpeg(quality).map_err(|e| error("FAILED", e))?;
        let id = format!("shot_{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let at = now_ms();
        let scale = rect.width as f64 / rgb.width as f64;
        self.remember(
            session,
            Shot {
                id: id.clone(),
                rect,
                width: rgb.width,
                height: rgb.height,
                at,
                layout: platform::layout(),
                target,
            },
        );
        note("正在看屏幕");
        Ok(json!({
            "shot": id, "width": rgb.width, "height": rgb.height, "scale": (scale * 1000.0).round() / 1000.0,
            "display": display, "at": at, "mimeType": "image/jpeg", "bytes": bytes.len(),
            "sha256": crate::protocol::sha256_hex(&bytes), "data": b64(&bytes),
        }))
    }

    /// `screenshot`: one picture of a display, a window or a region of an earlier picture.
    pub fn screenshot(&self, session: &str, args: &Value) -> Result<Value, DeviceError> {
        self.check_on()?;
        let target = self.target_of(session, args)?;
        let max_edge = int(args, "maxEdge")
            .map(|v| v.clamp(200, MAX_EDGE_MAX as i64) as usize)
            .unwrap_or(MAX_EDGE_DEFAULT);
        let grid = args.get("grid").and_then(Value::as_bool).unwrap_or(false);
        let quality = int(args, "quality").unwrap_or(75).clamp(30, 95) as u8;
        self.take(session, target, max_edge, grid, quality)
    }

    /// `windows`: the top-level windows and the displays. Private programs show no title.
    pub fn windows(&self) -> Result<Value, DeviceError> {
        self.check_on()?;
        let displays = platform::displays();
        let display_of = |r: &Rect| {
            displays
                .iter()
                .find(|d| d.rect.contains(r.x + r.width / 2, r.y + r.height / 2))
                .map(|d| d.index)
        };
        let list: Vec<Value> = platform::windows()
            .into_iter()
            .map(|w| {
                let private = self.is_private(&w.app);
                json!({
                    "window": window_ref(w.id),
                    "title": if private { "私密窗口".to_string() } else { clip(&w.title, 200) },
                    "app": w.app, "pid": w.pid, "display": display_of(&w.rect),
                    "minimized": w.minimized, "focused": w.focused, "private": private,
                })
            })
            .collect();
        let shown: Vec<Value> = displays
            .iter()
            .map(|d| json!({"display": d.index, "primary": d.primary, "width": d.rect.width, "height": d.rect.height}))
            .collect();
        Ok(json!({"windows": list, "displays": shown}))
    }

    fn point(&self, shot: &Shot, x: i64, y: i64) -> Result<(i64, i64), DeviceError> {
        if x < 0 || y < 0 || x > shot.width as i64 || y > shot.height as i64 {
            return Err(error(
                "OUT_OF_BOUNDS",
                format!(
                    "坐标 ({x}, {y}) 不在截图 {} 里（{}×{}）",
                    shot.id, shot.width, shot.height
                ),
            ));
        }
        let (px, py) = to_physical(shot, x, y);
        if let Some(bar) = platform::banner_area()
            && bar.contains(px, py)
        {
            return Err(DeviceError::denied(
                "这里是 AgentRouter 自己的提示条，AI 不能点",
            ));
        }
        if let Some(id) = platform::window_at(px, py) {
            let app = platform::process_name(platform::window_pid(id));
            if self.is_private(&app) {
                return Err(DeviceError::denied("这个位置是用户设成不给 AI 看的程序"));
            }
        }
        Ok((px, py))
    }

    /// `gui`: up to ten mouse and keyboard actions in order, coordinates in a screenshot's pixels.
    pub fn gui(&self, session: &str, scope: &Scope, args: &Value) -> Result<Value, DeviceError> {
        self.check_on()?;
        self.check_input(scope)?;
        let actions = args
            .get("actions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if actions.is_empty() || actions.len() > ACTIONS_MAX {
            return Err(error("FAILED", "actions 要写 1–10 个动作"));
        }
        let needs_shot = actions.iter().any(|a| {
            matches!(
                a.get("action").and_then(Value::as_str),
                Some("click" | "double_click" | "scroll" | "drag" | "move")
            )
        });
        let shot = if needs_shot {
            Some(self.shot(session, args.get("shot").and_then(Value::as_str))?)
        } else {
            None
        };
        if !platform::input_desktop_available() {
            return Err(secure_desktop());
        }
        if let Some(age) = platform::person_input_age_ms()
            && age < PERSON_ACTIVE_MS
        {
            return Err(
                error("USER_ACTIVE", "用户正在用这台电脑（刚动过鼠标或键盘）")
                    .next("等几秒再发；告诉用户你在等他停下。"),
            );
        }
        let mut done = Vec::new();
        for (i, a) in actions.iter().enumerate() {
            let kind = a.get("action").and_then(Value::as_str).unwrap_or("");
            let fail = |e: DeviceError| DeviceError {
                message: format!(
                    "第 {} 个动作（{kind}）没有做：{}（前 {i} 个已做完）",
                    i + 1,
                    e.message
                ),
                ..e
            };
            let line = self.one(shot.as_ref(), a).map_err(fail)?;
            note(&line);
            done.push(json!({"action": kind, "done": line}));
        }
        let mut out = json!({"done": done});
        if let Some(after) = args.get("screenshotAfter").filter(|v| !v.is_null()) {
            std::thread::sleep(Duration::from_millis(
                int(after, "delayMs").unwrap_or(500).clamp(0, 5000) as u64,
            ));
            let target = shot
                .map(|s| s.target)
                .unwrap_or_else(|| Target::Display(primary_index()));
            let max_edge = int(after, "maxEdge")
                .map(|v| v.clamp(200, MAX_EDGE_MAX as i64) as usize)
                .unwrap_or(MAX_EDGE_DEFAULT);
            let grid = after.get("grid").and_then(Value::as_bool).unwrap_or(false);
            out["screenshot"] = self.take(session, target, max_edge, grid, 75)?;
        }
        Ok(out)
    }

    fn one(&self, shot: Option<&Shot>, a: &Value) -> Result<String, DeviceError> {
        let kind = a.get("action").and_then(Value::as_str).unwrap_or("");
        let xy = |a: &Value| -> Result<(i64, i64), DeviceError> {
            let (Some(x), Some(y)) = (int(a, "x"), int(a, "y")) else {
                return Err(error("FAILED", "要写 x 和 y"));
            };
            self.point(shot.expect("coordinates need a shot"), x, y)
        };
        let io = |r: Result<(), String>| r.map_err(|_| secure_desktop());
        match kind {
            "move" => {
                let (x, y) = xy(a)?;
                io(platform::move_to(x, y))?;
                Ok(format!(
                    "移动鼠标（{}, {}）",
                    int(a, "x").unwrap_or(0),
                    int(a, "y").unwrap_or(0)
                ))
            }
            "click" | "double_click" => {
                let (x, y) = xy(a)?;
                let button = a.get("button").and_then(Value::as_str).unwrap_or("left");
                io(platform::move_to(x, y))?;
                std::thread::sleep(Duration::from_millis(40));
                let times = if kind == "double_click" { 2 } else { 1 };
                for t in 0..times {
                    if t > 0 {
                        std::thread::sleep(Duration::from_millis(70));
                    }
                    io(platform::button(button, true))?;
                    std::thread::sleep(Duration::from_millis(30));
                    io(platform::button(button, false))?;
                }
                let what = match (kind, button) {
                    ("double_click", _) => "双击",
                    (_, "right") => "右键点击",
                    (_, "middle") => "中键点击",
                    _ => "点击",
                };
                Ok(format!(
                    "{what}（{}, {}）",
                    int(a, "x").unwrap_or(0),
                    int(a, "y").unwrap_or(0)
                ))
            }
            "scroll" => {
                let (x, y) = xy(a)?;
                let (dx, dy) = (
                    int(a, "dx").unwrap_or(0).clamp(-30, 30),
                    int(a, "dy").unwrap_or(0).clamp(-30, 30),
                );
                io(platform::move_to(x, y))?;
                io(platform::scroll(dx, dy))?;
                Ok(format!("滚动（{dx}, {dy}）"))
            }
            "drag" => {
                let path: Vec<(i64, i64)> = a
                    .get("path")
                    .and_then(Value::as_array)
                    .map(|p| {
                        p.iter()
                            .filter_map(|pt| {
                                let arr = pt.as_array()?;
                                Some((arr.first()?.as_f64()? as i64, arr.get(1)?.as_f64()? as i64))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if path.len() < 2 || path.len() > 50 {
                    return Err(error("FAILED", "drag 的 path 要写 2–50 个点"));
                }
                let shot = shot.expect("drag needs a shot");
                let points: Vec<(i64, i64)> = path
                    .iter()
                    .map(|&(x, y)| self.point(shot, x, y))
                    .collect::<Result<_, _>>()?;
                io(platform::move_to(points[0].0, points[0].1))?;
                io(platform::button("left", true))?;
                for w in points.windows(2) {
                    let ((x0, y0), (x1, y1)) = (w[0], w[1]);
                    for s in 1..=8 {
                        std::thread::sleep(Duration::from_millis(12));
                        io(platform::move_to(
                            x0 + (x1 - x0) * s / 8,
                            y0 + (y1 - y0) * s / 8,
                        ))?;
                    }
                }
                io(platform::button("left", false))?;
                Ok(format!("拖动（{} 个点）", points.len()))
            }
            "type" => {
                let text = a.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() || text.chars().count() > 5000 {
                    return Err(error("FAILED", "type 要写 1–5000 个字的 text"));
                }
                if platform::focused_password() {
                    return Err(
                        error("PASSWORD_FIELD", "焦点在密码框里：AI 不往密码框里输入")
                            .next("告诉用户这一步需要他自己在电脑上输入。"),
                    );
                }
                io(platform::type_text(text))?;
                Ok(format!("输入文字（{} 个字）", text.chars().count()))
            }
            "key" => {
                let names: Vec<String> = a
                    .get("keys")
                    .and_then(Value::as_array)
                    .map(|k| {
                        k.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if names.is_empty() || names.len() > 5 {
                    return Err(error("FAILED", "key 要写 1–5 个键"));
                }
                let mut codes = Vec::new();
                for n in &names {
                    codes.push(
                        platform::virtual_key(n)
                            .ok_or_else(|| error("FAILED", format!("不认识的键：{n}")))?,
                    );
                }
                let lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
                let has = |k: &str| lower.iter().any(|n| n == k);
                if has("delete") && (has("ctrl") || has("control")) && has("alt") {
                    return Err(secure_desktop());
                }
                io(platform::press(&codes))?;
                Ok(format!("按下 {}", names.join("+")))
            }
            "wait" => {
                let ms = int(a, "ms").unwrap_or(1000).clamp(0, 5000);
                std::thread::sleep(Duration::from_millis(ms as u64));
                Ok(format!("等 {ms} 毫秒"))
            }
            other => Err(error("FAILED", format!("不认识的动作：{other}"))),
        }
    }

    /// `app`: open a GUI program (name, path, document) with a visible window, or close one.
    pub fn app(&self, session: &str, scope: &Scope, args: &Value) -> Result<Value, DeviceError> {
        self.check_on()?;
        self.check_input(scope)?;
        match args.get("op").and_then(Value::as_str).unwrap_or("") {
            "launch" => {
                let command = args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if command.is_empty() {
                    return Err(error("FAILED", "launch 要写 command（程序名或路径）"));
                }
                let params = args.get("args").and_then(Value::as_str).unwrap_or("");
                let cwd = match args
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
                {
                    Some(raw) => Some(scope.check(raw)?),
                    None => None,
                };
                if !platform::input_desktop_available() {
                    return Err(secure_desktop());
                }
                let before: Vec<u64> = platform::windows().into_iter().map(|w| w.id).collect();
                let pid = platform::launch(
                    command,
                    params,
                    cwd.as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .as_deref(),
                )
                .map_err(|e| {
                    error("NOT_FOUND", format!("打不开 {command}：{e}"))
                        .next("用完整路径，或先用 exec 找到程序装在哪。")
                })?;
                note(&format!("打开 {}", clip(command, 60)));
                // Wait for its window (a packaged app's starter exits; its window still shows up as new).
                let mut new = Vec::new();
                for _ in 0..40 {
                    std::thread::sleep(Duration::from_millis(250));
                    new = platform::windows()
                        .into_iter()
                        .filter(|w| !before.contains(&w.id) && !self.is_private(&w.app))
                        .collect::<Vec<_>>();
                    if new.iter().any(|w| Some(w.pid) == pid) || (!new.is_empty() && pid.is_none())
                    {
                        break;
                    }
                    if !new.is_empty() && pid.is_some_and(|p| !process_alive(p)) {
                        break;
                    }
                }
                let name = new
                    .first()
                    .map(|w| w.app.clone())
                    .unwrap_or_else(|| command.to_string());
                self.launched
                    .lock()
                    .unwrap()
                    .entry(session.to_string())
                    .or_default()
                    .push(Launched {
                        pid,
                        name: name.clone(),
                    });
                let windows: Vec<Value> = new
                    .iter()
                    .map(|w| json!({"window": window_ref(w.id), "title": clip(&w.title, 200), "app": w.app, "pid": w.pid}))
                    .collect();
                Ok(
                    json!({"launched": command, "pid": pid, "app": name, "windows": windows,
                    "note": if windows.is_empty() { "还没看到它的窗口：等一下再用 windows 看，或截一张图。" } else { "" }}),
                )
            }
            "close" => {
                let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
                let all = platform::windows();
                let window = parse_window(args.get("window"));
                let app = args.get("app").and_then(Value::as_str).unwrap_or("");
                let pid = int(args, "pid").map(|p| p as u32);
                let targets: Vec<Window> = all
                    .into_iter()
                    .filter(|w| Some(w.id) == window || same_app(app, &w.app) || Some(w.pid) == pid)
                    .collect();
                if targets.is_empty() && pid.is_none() {
                    return Err(error("NOT_FOUND", "没有找到要关的窗口")
                        .next("用 windows 看现在开着的窗口，按 window 或 app 关。"));
                }
                for w in &targets {
                    platform::close_window(w.id);
                }
                note("关闭程序");
                let ids: Vec<u64> = targets.iter().map(|w| w.id).collect();
                let mut left = ids.clone();
                for _ in 0..20 {
                    left.retain(|id| platform::is_window(*id));
                    if left.is_empty() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                let mut killed = Vec::new();
                if force {
                    let mut pids: Vec<u32> = targets
                        .iter()
                        .filter(|w| left.contains(&w.id))
                        .map(|w| w.pid)
                        .chain(pid)
                        .collect();
                    pids.sort();
                    pids.dedup();
                    for p in pids {
                        if p != std::process::id() && platform::kill(p) {
                            killed.push(p);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(300));
                    left.retain(|id| platform::is_window(*id));
                }
                Ok(json!({
                    "closed": ids.len() - left.len(),
                    "stillOpen": left.iter().map(|id| window_ref(*id)).collect::<Vec<_>>(),
                    "killed": killed,
                    "note": if left.is_empty() { "" } else { "还有窗口没关（程序可能在问要不要保存）：截一张图看看，或用 force:true 直接结束（没保存的内容会丢）。" },
                }))
            }
            "list" => {
                let launched = self
                    .launched
                    .lock()
                    .unwrap()
                    .get(session)
                    .cloned()
                    .unwrap_or_default();
                Ok(
                    json!({"launched": launched.iter().map(|l| json!({"app": l.name, "pid": l.pid})).collect::<Vec<_>>()}),
                )
            }
            _ => Err(error("FAILED", "op 要写 launch、close 或 list")),
        }
    }

    /// `screen_record`: a few frames at an interval, as one looping GIF (no streaming, §20).
    pub fn record(&self, session: &str, args: &Value) -> Result<Value, DeviceError> {
        self.check_on()?;
        let target = self.target_of(session, args)?;
        let frames = int(args, "frames").unwrap_or(10).clamp(2, 40) as usize;
        let interval = int(args, "intervalMs").unwrap_or(500).clamp(100, 2000) as u64;
        let max_edge = int(args, "maxEdge")
            .map(|v| v.clamp(200, MAX_EDGE_DEFAULT as i64) as usize)
            .unwrap_or(RECORD_EDGE_DEFAULT);
        let mut shots: Vec<image::Rgb> = Vec::with_capacity(frames);
        note(&format!("正在录屏（{frames} 帧）"));
        for i in 0..frames {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(interval));
            }
            let (_, rgb, _) = self.grab(&target, max_edge)?;
            if let Some(first) = shots.first()
                && (first.width != rgb.width || first.height != rgb.height)
            {
                break; // the window was resized: keep what we have
            }
            shots.push(rgb);
        }
        let gif = image::gif(&shots, interval as u32).map_err(|e| error("FAILED", e))?;
        if gif.len() > IMAGE_BYTES_MAX {
            return Err(error(
                "TOO_LARGE",
                format!("录屏有 {} MB，太大了", gif.len() / 1_000_000),
            )
            .next("少录几帧，或把 max_edge 调小（例如 640）。"));
        }
        Ok(json!({
            "mimeType": "image/gif", "frames": shots.len(), "intervalMs": interval,
            "width": shots[0].width, "height": shots[0].height, "bytes": gif.len(),
            "sha256": crate::protocol::sha256_hex(&gif), "data": b64(&gif),
        }))
    }

    /// The conversation ended or the person disconnected: forget its screenshots.
    pub fn forget(&self) {
        self.shots.lock().unwrap().clear();
    }
}

fn secure_desktop() -> DeviceError {
    error(
        "SECURE_DESKTOP",
        "电脑现在在锁屏、UAC 或安全桌面上，AI 不能在那里操作",
    )
    .next("告诉用户这一步需要他自己在电脑上点。")
}

fn to_physical(shot: &Shot, x: i64, y: i64) -> (i64, i64) {
    let sx = shot.rect.width as f64 / shot.width.max(1) as f64;
    let sy = shot.rect.height as f64 / shot.height.max(1) as f64;
    (
        shot.rect.x + (x as f64 * sx).round() as i64,
        shot.rect.y + (y as f64 * sy).round() as i64,
    )
}

fn primary_index() -> usize {
    platform::displays()
        .iter()
        .position(|d| d.primary)
        .unwrap_or(0)
}

fn note(text: &str) {
    platform::note(text);
}

fn process_alive(pid: u32) -> bool {
    !platform::process_name(pid).is_empty()
}

pub fn supported() -> bool {
    cfg!(windows)
}

/// The lock screen, UAC or another secure desktop is up (nothing acts there; local MCP servers included).
pub fn secure_desktop_active() -> bool {
    supported() && !platform::input_desktop_available()
}

/// The 「正在操作」 bar for actions that are not the screen tools' own (a local MCP server's call).
pub fn note_banner(text: &str) {
    if supported() {
        note(text);
    }
}

#[cfg(windows)]
mod platform {
    pub use super::win::*;
    use super::{Display, Rect};

    pub fn layout() -> String {
        let v = virtual_screen();
        let d: Vec<Display> = displays();
        format!(
            "{},{},{},{}|{}",
            v.x,
            v.y,
            v.width,
            v.height,
            d.iter()
                .map(|d| format!(
                    "{}:{}:{}:{}",
                    d.rect.x, d.rect.y, d.rect.width, d.rect.height
                ))
                .collect::<Vec<_>>()
                .join(";")
        )
    }
    pub fn note(text: &str) {
        super::banner::note(text);
    }
    pub fn banner_area() -> Option<Rect> {
        super::banner::area()
    }
}

/// Elsewhere every screen action answers UNSUPPORTED before reaching these.
#[cfg(not(windows))]
mod platform {
    use super::image::Frame;
    use super::{Display, Rect, Window};
    pub fn layout() -> String {
        String::new()
    }
    pub fn note(_: &str) {}
    pub fn banner_area() -> Option<Rect> {
        None
    }
    pub fn displays() -> Vec<Display> {
        Vec::new()
    }
    pub fn windows() -> Vec<Window> {
        Vec::new()
    }
    pub fn virtual_screen() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }
    pub fn capture(_: Rect) -> Result<Frame, String> {
        Err("unsupported".into())
    }
    pub fn input_desktop_available() -> bool {
        false
    }
    pub fn is_window(_: u64) -> bool {
        false
    }
    pub fn focus(_: u64) {}
    pub fn window_bounds(_: u64) -> Option<Rect> {
        None
    }
    pub fn window_at(_: i64, _: i64) -> Option<u64> {
        None
    }
    pub fn window_pid(_: u64) -> u32 {
        0
    }
    pub fn process_name(_: u32) -> String {
        String::new()
    }
    pub fn person_input_age_ms() -> Option<u32> {
        None
    }
    pub fn move_to(_: i64, _: i64) -> Result<(), String> {
        Err("unsupported".into())
    }
    pub fn button(_: &str, _: bool) -> Result<(), String> {
        Err("unsupported".into())
    }
    pub fn scroll(_: i64, _: i64) -> Result<(), String> {
        Err("unsupported".into())
    }
    pub fn type_text(_: &str) -> Result<(), String> {
        Err("unsupported".into())
    }
    pub fn virtual_key(_: &str) -> Option<u16> {
        None
    }
    pub fn press(_: &[u16]) -> Result<(), String> {
        Err("unsupported".into())
    }
    pub fn focused_password() -> bool {
        false
    }
    pub fn launch(_: &str, _: &str, _: Option<&str>) -> Result<Option<u32>, String> {
        Err("unsupported".into())
    }
    pub fn close_window(_: u64) -> bool {
        false
    }
    pub fn kill(_: u32) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_map_back_to_physical_pixels() {
        let shot = Shot {
            id: "shot_1".into(),
            rect: Rect {
                x: -1920,
                y: 0,
                width: 2560,
                height: 1440,
            },
            width: 1280,
            height: 720,
            at: 0,
            layout: String::new(),
            target: Target::Display(1),
        };
        assert_eq!(to_physical(&shot, 0, 0), (-1920, 0));
        assert_eq!(to_physical(&shot, 640, 360), (-640, 720));
    }

    #[test]
    fn app_names_match_with_or_without_exe() {
        assert!(same_app("KeePass", "keepass.exe"));
        assert!(same_app("WeChat.exe", "wechat.EXE"));
        assert!(!same_app("", "x.exe"));
        assert!(!same_app("note", "notepad.exe"));
    }

    #[test]
    fn window_refs_round_trip() {
        assert_eq!(parse_window(Some(&json!(window_ref(4242)))), Some(4242));
        assert_eq!(parse_window(Some(&json!(17))), Some(17));
    }

    #[test]
    fn screen_is_off_until_the_person_turns_it_on() {
        let s = Screen::default();
        let e = s.windows().unwrap_err();
        assert!(e.code == "SCREEN_OFF" || e.code == "UNSUPPORTED");
    }
}
