//! Messages sent over the `control` data channel (see ARCHITECTURE.md §5):
//! host -> client cursor shape/visibility, plus an application-level
//! ping/pong the client uses to measure its own end-to-end RTT (distinct
//! from the WebRTC-level RTT already shown in the stats overlay, see
//! ARCHITECTURE.md §4.2).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A single capturable display, as shown in the client's display picker
/// (slice 2.4). `width`/`height` are the display's size in its own
/// OS-global units (points on macOS, physical pixels on Windows) -- for
/// labelling in the UI only, not a frame/pixel size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DisplayEntry {
    pub id: u32,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// The owner's chosen quality preset (slice 3.5e), sent to the host in
/// `ControlMessage::SetQuality` and interpreted by the adaptation controller
/// (`host::adapt::Controller::set_preset`): `Auto` is the controller's
/// original feedback-driven behavior, `Sharp` favors legible text (a lower
/// fps ceiling, a bigger per-frame bit budget), `Smooth` favors motion (a
/// higher fps floor, a smaller per-frame bit budget). See
/// `host/src/adapt/mod.rs` for the exact numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum QualityPreset {
    Auto,
    Sharp,
    Smooth,
}

/// The host's operating system, as reported in `ControlMessage::HostInfo`
/// (slice 3.5f). Used by the client to decide whether Cmd should be remapped
/// to Ctrl (`web/src/keyRemap.ts`'s `cmdAsCtrlApplies`: a Mac client talking
/// to a Windows host, where the physical Cmd key would otherwise arrive as
/// the Windows key -- see that module's doc comment for the full story).
/// `Other` covers every non-macOS, non-Windows target (only relevant to
/// `cargo test --workspace` on CI's `ubuntu-latest` job -- `rcdesk-agent`
/// itself only ships for macOS/Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum HostOs {
    Macos,
    Windows,
    Other,
}

/// Этап обновления хоста из сеанса (слайс D50d), см.
/// `ControlMessage::UpdateStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum UpdateState {
    Checking,
    Downloading,
    Installing,
    UpToDate,
    Failed,
}

/// A single `control`-channel message, JSON-encoded on the wire (see
/// ARCHITECTURE.md §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ControlMessage {
    /// host -> client. The current shape of the system cursor. `rgba` is
    /// base64 (no line breaks) of raw RGBA bytes, `width * height * 4` of
    /// them, straight (not premultiplied) alpha -- not required, since the
    /// client only ever hands the bytes to the browser's own PNG encoder,
    /// which doesn't care either way. `scale` is how many times larger the
    /// image is than its logical size (1, or 2 on a Retina display);
    /// `hotspot_x`/`hotspot_y` are in logical points, matching `scale`.
    CursorShape {
        width: u32,
        height: u32,
        hotspot_x: f64,
        hotspot_y: f64,
        scale: f64,
        rgba: String,
    },
    /// host -> client: the system cursor is hidden (e.g. while typing).
    CursorHidden,
    /// client -> host; the host echoes back `Pong` with the same `ts` so the
    /// client can compute an application-level round trip (see `app.ts`'s
    /// overlay `app N ms`).
    Ping { ts: f64 },
    /// host -> client, in reply to `Ping`.
    Pong { ts: f64 },
    /// host -> client: the adaptation controller changed the encoder's rate target
    /// (slice 2.3). Shown in the client's stats overlay; informational only.
    Quality {
        bitrate_kbps: u32,
        fps: u32,
        reason: String,
    },
    /// host -> client: the list of displays the host can capture, and the id
    /// of the one currently being streamed (slice 2.4). Sent when the
    /// `control` channel opens and again after every display switch.
    Displays {
        displays: Vec<DisplayEntry>,
        current: u32,
    },
    /// client -> host: switch the streamed display to `id`.
    SelectDisplay { id: u32 },
    /// host -> client: whether the host can currently inject mouse/keyboard
    /// input on this session (slice 2.5a, closing debt D26). Sent once,
    /// right after `Displays`, when the `control` channel opens -- session
    /// start is the only point input availability is decided, so unlike
    /// `Displays` this never needs to be re-sent later. `available: false`
    /// means the session is view-only (e.g. the host is missing the macOS
    /// Accessibility permission, or was started with `serve --no-input`);
    /// `reason` is a human-readable explanation for display, or `None` when
    /// there's nothing to add (`available: true`).
    InputStatus {
        available: bool,
        reason: Option<String>,
    },
    /// host -> client: the Windows foreground window belongs to a process
    /// running at a higher integrity level than the host agent's own
    /// (slice 2.6e, e.g. Task Manager opened "as administrator") -- UIPI
    /// silently drops `SendInput` events aimed at it, so clicks/keys the
    /// client sends while this window is focused have no effect. **Not**
    /// the same thing as `InputStatus`: that one means there is no input
    /// injector at all for the whole session, and the client detaches
    /// input entirely. `InputBlocked` is a transient, window-by-window
    /// condition -- the client keeps sending input as usual (so the user
    /// can still click away from the blocking window) and only shows a
    /// warning banner. Sent whenever the host's foreground-window watcher
    /// (polled once a second, Windows only) observes a change; a session
    /// that never sends this is never blocked. `reason` is set to a
    /// human-readable explanation when `blocked` is `true`, `None` when
    /// `blocked` is `false`. The full-control mode (phase 4.1, host running
    /// as a service) doesn't have this limitation.
    InputBlocked {
        blocked: bool,
        reason: Option<String>,
    },
    /// host -> client: the host's clipboard text changed (slice 2.5b),
    /// polled every 250ms via an OS change counter (`NSPasteboard`
    /// `changeCount` / `GetClipboardSequenceNumber`, see `host::clipboard`).
    /// `text` is never longer than `host::clipboard::MAX_CLIPBOARD_BYTES`
    /// (200_000 UTF-8 bytes) -- anything bigger is dropped on the host with
    /// a `warn` (length only, never content). Sent on the `control` channel,
    /// never on `input`, since it isn't an input event; contrast
    /// `proto::input::InputMessage::ClipboardText`, the client -> host
    /// direction, which *does* need to travel on `input` (see that variant's
    /// doc comment for why).
    ClipboardText { text: String },
    /// client -> host: switch the adaptation controller's quality preset
    /// (slice 3.5e). Applies to the session's current pipeline and survives
    /// a later `SelectDisplay` (the host re-applies it to the rebuilt
    /// pipeline's controller). With `--no-adapt`, the host ignores this with
    /// a `debug` log -- there is no controller to switch.
    SetQuality { preset: QualityPreset },
    /// host -> client: the host's operating system (slice 3.5f). Sent once,
    /// alongside `Displays`, when the `control` channel opens -- like
    /// `InputStatus`, this never changes mid-session, so it's never re-sent
    /// later. `files_enabled` (slice 3.4a) is `false` when the host was
    /// started with `serve --no-files`: the client hides its file-transfer
    /// button instead of letting every transfer be refused.
    HostInfo {
        os: HostOs,
        files_enabled: bool,
        /// Шторка доступна на этом хосте (слайс 4.2c) -- сейчас только
        /// агент macOS. Клиент показывает кнопку шторки только при `true`.
        curtain_available: bool,
        /// Хост умеет переключать раскладку клавиатуры (слайс 4.6c) --
        /// сейчас только агент macOS. Старый JSON без поля -- `false`.
        #[serde(default)]
        input_source_available: bool,
        /// Версия хоста `X.Y.Z` (слайс D50a); старые хосты поле не шлют.
        #[serde(default)]
        version: Option<String>,
        /// Хост умеет обновиться из сеанса (слайс D50d). Старый JSON без
        /// поля -- `false`.
        #[serde(default)]
        update_available: bool,
        /// Хост умеет послать Ctrl+Alt+Del (слайс D46) -- только полный
        /// режим Windows. Старый JSON без поля -- `false`.
        #[serde(default)]
        ctrl_alt_del_available: bool,
    },
    /// client -> host: включить (`on: true`) или выключить шторку (слайс
    /// 4.2c). Шторка закрывает физический монитор хоста чёрным окном
    /// (в видео оно не попадает) и блокирует локальный ввод. Хост всегда
    /// отвечает `CurtainStatus`.
    SetCurtain { on: bool },
    /// host -> client: итоговое состояние шторки (слайс 4.2c). Шлётся в
    /// ответ на каждый `SetCurtain` и когда хост снял шторку сам, из-за
    /// ошибки. `reason` -- человекочитаемая причина, когда запрошенное
    /// состояние не достигнуто (или шторка снята по ошибке), иначе `None`.
    CurtainStatus { on: bool, reason: Option<String> },
    /// client -> host: переключить раскладку клавиатуры хоста на следующую
    /// (слайс 4.6c). Хост отвечает `InputSource` с новой меткой.
    SwitchInputSource,
    /// client -> host: послать Ctrl+Alt+Del (слайс D46). Из браузера эту
    /// комбинацию передать нельзя, хост вызывает `SendSAS`. Работает только
    /// в полном режиме Windows; ответа нет.
    SendCtrlAltDel,
    /// host -> client: метка текущей раскладки хоста (слайс 4.6c), например
    /// `"RU"`. Шлётся после `HostInfo`, в ответ на `SwitchInputSource` и при
    /// любом изменении раскладки на самом хосте.
    InputSource { label: String },
    /// client -> host: обновить хост до последней версии релиза (слайс
    /// D50d). Хост отвечает потоком `UpdateStatus`.
    UpdateHost,
    /// host -> client: ход обновления (слайс D50d). `version` -- целевая
    /// версия (`Downloading`/`Installing`/`UpToDate`), `message` -- причина
    /// при `Failed`. `received`/`total` -- байты установщика при
    /// `Downloading` (D81); `total` неизвестен, если сервер не сообщил размер.
    UpdateStatus {
        state: UpdateState,
        version: Option<String>,
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(type = "number | null", optional)]
        received: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(type = "number | null", optional)]
        total: Option<u64>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_shape_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::CursorShape {
            width: 16,
            height: 16,
            hotspot_x: 1.5,
            hotspot_y: 2.0,
            scale: 2.0,
            rgba: "QUJD".to_string(),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"cursor_shape","width":16,"height":16,"hotspot_x":1.5,"hotspot_y":2.0,"scale":2.0,"rgba":"QUJD"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn cursor_hidden_round_trips_through_json() {
        let msg = ControlMessage::CursorHidden;

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"cursor_hidden"}"#);

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn ping_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::Ping { ts: 1234.5 };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"ping","ts":1234.5}"#);

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn pong_round_trips_through_json() {
        let msg = ControlMessage::Pong { ts: 42.0 };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"pong","ts":42.0}"#);

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn quality_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::Quality {
            bitrate_kbps: 3200,
            fps: 20,
            reason: "remb".to_string(),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"quality","bitrate_kbps":3200,"fps":20,"reason":"remb"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn displays_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::Displays {
            displays: vec![DisplayEntry {
                id: 1,
                title: "Built-in".to_string(),
                width: 1728,
                height: 1117,
                primary: true,
            }],
            current: 1,
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"displays","displays":[{"id":1,"title":"Built-in","width":1728,"height":1117,"primary":true}],"current":1}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn select_display_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::SelectDisplay { id: 2 };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"select_display","id":2}"#);

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn input_status_unavailable_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::InputStatus {
            available: false,
            reason: Some("no Accessibility permission".to_string()),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"input_status","available":false,"reason":"no Accessibility permission"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn input_status_available_serializes_reason_as_json_null() {
        let msg = ControlMessage::InputStatus {
            available: true,
            reason: None,
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"input_status","available":true,"reason":null}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn input_blocked_true_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::InputBlocked {
            blocked: true,
            reason: Some(
                "Foreground window runs with administrator rights; input is blocked by Windows (UIPI)"
                    .to_string(),
            ),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"input_blocked","blocked":true,"reason":"Foreground window runs with administrator rights; input is blocked by Windows (UIPI)"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn input_blocked_false_serializes_reason_as_json_null() {
        let msg = ControlMessage::InputBlocked {
            blocked: false,
            reason: None,
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"input_blocked","blocked":false,"reason":null}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn clipboard_text_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::ClipboardText {
            text: "hello from host".to_string(),
        };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"clipboard_text","text":"hello from host"}"#
        );

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn set_quality_round_trips_through_json_with_exact_shape() {
        for (preset, wire) in [
            (QualityPreset::Auto, "auto"),
            (QualityPreset::Sharp, "sharp"),
            (QualityPreset::Smooth, "smooth"),
        ] {
            let msg = ControlMessage::SetQuality { preset };

            let json = serde_json::to_string(&msg).expect("serialize");
            assert_eq!(
                json,
                format!(r#"{{"type":"set_quality","preset":"{wire}"}}"#)
            );

            let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round_tripped, msg);
        }
    }

    #[test]
    fn host_info_round_trips_through_json_with_exact_shape() {
        for (os, wire) in [
            (HostOs::Macos, "macos"),
            (HostOs::Windows, "windows"),
            (HostOs::Other, "other"),
        ] {
            let msg = ControlMessage::HostInfo {
                os,
                files_enabled: true,
                curtain_available: false,
                input_source_available: true,
                version: Some("0.1.1".to_string()),
                update_available: true,
                ctrl_alt_del_available: false,
            };

            let json = serde_json::to_string(&msg).expect("serialize");
            assert_eq!(
                json,
                format!(
                    r#"{{"type":"host_info","os":"{wire}","files_enabled":true,"curtain_available":false,"input_source_available":true,"version":"0.1.1","update_available":true,"ctrl_alt_del_available":false}}"#
                )
            );

            let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round_tripped, msg);
        }
    }

    #[test]
    fn set_curtain_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::SetCurtain { on: true };

        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"set_curtain","on":true}"#);

        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn curtain_status_round_trips_through_json_with_exact_shape() {
        let off = ControlMessage::CurtainStatus {
            on: false,
            reason: Some("нет прав".to_string()),
        };
        let json = serde_json::to_string(&off).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"curtain_status","on":false,"reason":"нет прав"}"#
        );
        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, off);

        let on = ControlMessage::CurtainStatus {
            on: true,
            reason: None,
        };
        let json = serde_json::to_string(&on).expect("serialize");
        assert_eq!(json, r#"{"type":"curtain_status","on":true,"reason":null}"#);
        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, on);
    }

    #[test]
    fn host_info_without_input_source_available_parses_as_false() {
        let json =
            r#"{"type":"host_info","os":"macos","files_enabled":true,"curtain_available":true}"#;
        let msg: ControlMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            msg,
            ControlMessage::HostInfo {
                os: HostOs::Macos,
                files_enabled: true,
                curtain_available: true,
                input_source_available: false,
                version: None,
                update_available: false,
                ctrl_alt_del_available: false,
            }
        );
    }

    /// Слайс D46: JSON без `ctrl_alt_del_available` разбирается в `false`.
    #[test]
    fn host_info_without_ctrl_alt_del_available_parses_as_false() {
        let json = r#"{"type":"host_info","os":"windows","files_enabled":true,"curtain_available":false,"input_source_available":false,"version":"0.2.3","update_available":false}"#;
        let msg: ControlMessage = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(
            msg,
            ControlMessage::HostInfo {
                ctrl_alt_del_available: false,
                ..
            }
        ));
    }

    /// Слайс D46: `ctrl_alt_del_available: true` переживает round-trip.
    #[test]
    fn host_info_with_ctrl_alt_del_available_round_trips() {
        let json = r#"{"type":"host_info","os":"windows","files_enabled":true,"curtain_available":false,"input_source_available":false,"version":"0.2.3","update_available":false,"ctrl_alt_del_available":true}"#;
        let msg: ControlMessage = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(
            msg,
            ControlMessage::HostInfo {
                ctrl_alt_del_available: true,
                ..
            }
        ));
        assert_eq!(serde_json::to_string(&msg).expect("serialize"), json);
    }

    /// Слайс D46: `send_ctrl_alt_del` без полей.
    #[test]
    fn send_ctrl_alt_del_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::SendCtrlAltDel;
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"send_ctrl_alt_del"}"#);
        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    /// Слайс D50a: JSON без `version` (хосты 0.1.0) разбирается в `None`.
    #[test]
    fn host_info_without_version_parses_as_none() {
        let json = r#"{"type":"host_info","os":"macos","files_enabled":true,"curtain_available":true,"input_source_available":false}"#;
        let msg: ControlMessage = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(
            msg,
            ControlMessage::HostInfo { version: None, .. }
        ));
    }

    /// Слайс D50a: `version` переживает круговой round-trip.
    #[test]
    fn host_info_with_version_round_trips() {
        let json = r#"{"type":"host_info","os":"windows","files_enabled":true,"curtain_available":false,"input_source_available":false,"version":"0.2.3","update_available":false,"ctrl_alt_del_available":false}"#;
        let msg: ControlMessage = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(
            &msg,
            ControlMessage::HostInfo { version: Some(v), .. } if v == "0.2.3"
        ));
        let again = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(again, json);
    }

    #[test]
    fn switch_input_source_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::SwitchInputSource;
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"switch_input_source"}"#);
        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn input_source_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::InputSource {
            label: "RU".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"input_source","label":"RU"}"#);
        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    /// Слайс D50d: `update_host` без полей.
    #[test]
    fn update_host_round_trips_through_json_with_exact_shape() {
        let msg = ControlMessage::UpdateHost;
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"type":"update_host"}"#);
        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    /// Слайс D50d: точная форма `update_status` для каждого состояния.
    #[test]
    fn update_status_round_trips_through_json_with_exact_shape() {
        for (state, wire) in [
            (UpdateState::Checking, "checking"),
            (UpdateState::Downloading, "downloading"),
            (UpdateState::Installing, "installing"),
            (UpdateState::UpToDate, "up_to_date"),
            (UpdateState::Failed, "failed"),
        ] {
            let msg = ControlMessage::UpdateStatus {
                state,
                version: Some("0.2.0".to_string()),
                message: None,
                received: None,
                total: None,
            };
            let json = serde_json::to_string(&msg).expect("serialize");
            assert_eq!(
                json,
                format!(
                    r#"{{"type":"update_status","state":"{wire}","version":"0.2.0","message":null}}"#
                )
            );
            let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round_tripped, msg);
        }
        let failed = ControlMessage::UpdateStatus {
            state: UpdateState::Failed,
            version: None,
            message: Some("нет сети".to_string()),
            received: None,
            total: None,
        };
        let json = serde_json::to_string(&failed).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"update_status","state":"failed","version":null,"message":"нет сети"}"#
        );
    }

    /// D81: прогресс загрузки несёт `received`/`total`; без них поля нет.
    #[test]
    fn update_status_with_progress_has_exact_shape() {
        let msg = ControlMessage::UpdateStatus {
            state: UpdateState::Downloading,
            version: Some("0.3.3".to_string()),
            message: None,
            received: Some(4_404_019),
            total: Some(13_900_000),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"update_status","state":"downloading","version":"0.3.3","message":null,"received":4404019,"total":13900000}"#
        );
        let round_tripped: ControlMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    /// Слайс D50d: старый `host_info` без `update_available` -- `false`.
    #[test]
    fn host_info_without_update_available_parses_as_false() {
        let json = r#"{"type":"host_info","os":"macos","files_enabled":true,"curtain_available":true,"input_source_available":false,"version":"0.2.0"}"#;
        let msg: ControlMessage = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(
            msg,
            ControlMessage::HostInfo {
                update_available: false,
                ..
            }
        ));
    }
}
