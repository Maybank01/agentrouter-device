//! A live check of the screen actions on a real Windows desktop (opens Notepad, types, closes it).
//! Not run in CI: `cargo test -p agentrouter-device-core --test screen_live -- --ignored --nocapture`.

#![cfg(windows)]

use agentrouter_device::config::Access;
use agentrouter_device::gate::Scope;
use agentrouter_device::screen::Screen;
use serde_json::json;

#[test]
#[ignore]
fn notepad_round_trip() {
    let screen = Screen::default();
    screen.set(true, &[]);
    let scope = Scope {
        access: Access::Folders,
        folders: vec![std::env::temp_dir()],
        deny: vec![],
    };
    let shot = screen
        .screenshot("s", &json!({"maxEdge": 1280, "grid": true}))
        .unwrap();
    println!(
        "shot {} {}x{} {} bytes",
        shot["shot"], shot["width"], shot["height"], shot["bytes"]
    );
    let launched = screen
        .app("s", &scope, &json!({"op": "launch", "command": "notepad"}))
        .unwrap();
    println!("launched {launched}");
    let window = launched["windows"][0]["window"]
        .as_str()
        .expect("a notepad window")
        .to_string();
    let shot = screen
        .screenshot("s", &json!({"target": "window", "window": window}))
        .unwrap();
    let (w, h) = (
        shot["width"].as_i64().unwrap(),
        shot["height"].as_i64().unwrap(),
    );
    let done = screen
        .gui(
            "s",
            &scope,
            &json!({"actions": [
                {"action": "click", "x": w / 2, "y": h / 2},
                {"action": "type", "text": "hello from AgentRouter"},
                {"action": "key", "keys": ["ctrl", "a"]},
            ], "screenshotAfter": {"maxEdge": 800}}),
        )
        .unwrap();
    println!(
        "gui {} / after {} bytes",
        done["done"], done["screenshot"]["bytes"]
    );
    let gif = screen
        .record(
            "s",
            &json!({"target": "window", "window": window, "frames": 3, "intervalMs": 200}),
        )
        .unwrap();
    println!("gif {} frames {} bytes", gif["frames"], gif["bytes"]);
    let closed = screen
        .app(
            "s",
            &scope,
            &json!({"op": "close", "window": window, "force": true}),
        )
        .unwrap();
    println!("closed {closed}");
}
