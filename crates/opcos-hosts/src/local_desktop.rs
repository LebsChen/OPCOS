use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use opcos_computer_use::{ComputerUseAction, ComputerUseResponse, ScreenBounds, Screenshot};
use std::sync::Mutex;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use xcap::Monitor;

#[derive(Debug)]
pub struct LocalDesktop {
    input: Mutex<Option<Enigo>>,
}

impl LocalDesktop {
    pub fn new() -> Self {
        let input = if wayland_session() {
            None
        } else {
            Enigo::new(&Settings::default()).ok()
        };
        Self {
            input: Mutex::new(input),
        }
    }

    pub fn capability_reasons(&self) -> (Option<String>, Option<String>) {
        if wayland_session() {
            let reason: String = "Wayland session detected; local screenshot/input requires an XDG Desktop Portal Remote Desktop session, which is not configured".into();
            return (Some(reason.clone()), Some(reason));
        }
        let screenshot = match self.capture_bounds() {
            Ok(_) => None,
            Err(error) => Some(format!("local screen capture unavailable: {error}")),
        };
        let input = self
            .input
            .lock()
            .map_err(|_| "input backend lock is poisoned".to_owned())
            .ok()
            .and_then(|input| input.as_ref().map(|_| ()))
            .map_or_else(|| Some(input_error_reason()), |_| None);
        (screenshot, input)
    }

    pub fn screenshot(&self) -> Result<Screenshot, String> {
        if wayland_session() {
            return Err("Wayland screen capture is unavailable; configure an XDG Desktop Portal screen-cast session".into());
        }
        let (width, height, pixels) = self.capture_rgba()?;
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .and_then(|mut writer| writer.write_image_data(&pixels))
            .map_err(|error| capture_error(error.to_string()))?;
        Ok(Screenshot {
            image: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
            format: "png".into(),
        })
    }

    pub fn computer_use(
        &self,
        action: ComputerUseAction,
        bounds: ScreenBounds,
    ) -> Result<ComputerUseResponse, String> {
        action.validate(bounds).map_err(|error| error.to_string())?;
        if wayland_session() {
            return Err("Wayland input injection is unavailable; authorize an XDG Desktop Portal Remote Desktop session".into());
        }
        let mut input = self
            .input
            .lock()
            .map_err(|_| "local input backend lock is poisoned".to_owned())?;
        let input = input.as_mut().ok_or_else(input_error_reason)?;
        match action {
            ComputerUseAction::Screenshot | ComputerUseAction::Wait => {}
            ComputerUseAction::CursorPosition => {
                let (x, y) = input.location().map_err(|error| error.to_string())?;
                return Ok(ComputerUseResponse {
                    ok: true,
                    coordinate: Some([x, y]),
                    x: Some(x),
                    y: Some(y),
                    error: None,
                });
            }
            ComputerUseAction::Type { text } => {
                input.text(&text).map_err(|error| error.to_string())?
            }
            ComputerUseAction::Key { key } => {
                let key = parse_key(&key)?;
                input
                    .key(key, Direction::Click)
                    .map_err(|error| error.to_string())?;
            }
            ComputerUseAction::HoldKey { key } => {
                input
                    .key(parse_key(&key)?, Direction::Press)
                    .map_err(|error| error.to_string())?;
            }
            ComputerUseAction::MouseMove { coordinate } => {
                input
                    .move_mouse(coordinate[0], coordinate[1], Coordinate::Abs)
                    .map_err(|error| error.to_string())?;
            }
            ComputerUseAction::Scroll {
                direction, amount, ..
            } => {
                let axis = if matches!(direction.as_str(), "left" | "right") {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                let value = if matches!(direction.as_str(), "up" | "right") {
                    amount
                } else {
                    -amount
                };
                input
                    .scroll(value, axis)
                    .map_err(|error| error.to_string())?;
            }
            ComputerUseAction::LeftClick { coordinate }
            | ComputerUseAction::RightClick { coordinate }
            | ComputerUseAction::MiddleClick { coordinate }
            | ComputerUseAction::DoubleClick { coordinate }
            | ComputerUseAction::TripleClick { coordinate } => {
                input
                    .move_mouse(coordinate[0], coordinate[1], Coordinate::Abs)
                    .map_err(|error| error.to_string())?;
                let button = match action {
                    ComputerUseAction::RightClick { .. } => Button::Right,
                    ComputerUseAction::MiddleClick { .. } => Button::Middle,
                    _ => Button::Left,
                };
                let count = match action {
                    ComputerUseAction::DoubleClick { .. } => 2,
                    ComputerUseAction::TripleClick { .. } => 3,
                    _ => 1,
                };
                for _ in 0..count {
                    input
                        .button(button, Direction::Click)
                        .map_err(|error| error.to_string())?;
                }
            }
            ComputerUseAction::LeftClickDrag {
                coordinate,
                coordinate_end,
            } => {
                input
                    .move_mouse(coordinate[0], coordinate[1], Coordinate::Abs)
                    .map_err(|error| error.to_string())?;
                input
                    .button(Button::Left, Direction::Press)
                    .map_err(|error| error.to_string())?;
                input
                    .move_mouse(coordinate_end[0], coordinate_end[1], Coordinate::Abs)
                    .map_err(|error| error.to_string())?;
                input
                    .button(Button::Left, Direction::Release)
                    .map_err(|error| error.to_string())?;
            }
            ComputerUseAction::LeftMouseDown { coordinate } => {
                input
                    .move_mouse(coordinate[0], coordinate[1], Coordinate::Abs)
                    .map_err(|error| error.to_string())?;
                input
                    .button(Button::Left, Direction::Press)
                    .map_err(|error| error.to_string())?;
            }
            ComputerUseAction::LeftMouseUp { coordinate } => {
                input
                    .move_mouse(coordinate[0], coordinate[1], Coordinate::Abs)
                    .map_err(|error| error.to_string())?;
                input
                    .button(Button::Left, Direction::Release)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(ComputerUseResponse {
            ok: true,
            coordinate: None,
            x: None,
            y: None,
            error: None,
        })
    }

    fn capture_bounds(&self) -> Result<(u32, u32), String> {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            let monitor = Monitor::all()
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|monitor| monitor.is_primary().unwrap_or(false))
                .or_else(|| Monitor::all().ok().and_then(|mut monitors| monitors.pop()))
                .ok_or_else(|| "no local display monitor was detected".to_owned())?;
            return Ok((
                monitor.width().map_err(|error| error.to_string())?,
                monitor.height().map_err(|error| error.to_string())?,
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use x11rb::connection::Connection;
            let (connection, screen) = x11rb::connect(None).map_err(|error| error.to_string())?;
            let root = &connection.setup().roots[screen];
            return Ok((root.width_in_pixels.into(), root.height_in_pixels.into()));
        }
        #[allow(unreachable_code)]
        Err("screen capture is unavailable on this platform".into())
    }

    fn capture_rgba(&self) -> Result<(u32, u32, Vec<u8>), String> {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            let monitor = Monitor::all()
                .map_err(|error| capture_error(error.to_string()))?
                .into_iter()
                .find(|monitor| monitor.is_primary().unwrap_or(false))
                .or_else(|| Monitor::all().ok().and_then(|mut monitors| monitors.pop()))
                .ok_or_else(|| {
                    "local screen capture unavailable: no monitor detected".to_owned()
                })?;
            let image = monitor
                .capture_image()
                .map_err(|error| capture_error(error.to_string()))?;
            return Ok((image.width(), image.height(), image.as_raw().to_vec()));
        }
        #[cfg(target_os = "linux")]
        {
            use x11rb::{
                connection::Connection,
                protocol::xproto::{ConnectionExt, ImageFormat},
            };
            let (connection, screen) = x11rb::connect(None).map_err(|error| error.to_string())?;
            let root = &connection.setup().roots[screen];
            let width = root.width_in_pixels as u32;
            let height = root.height_in_pixels as u32;
            let data = connection
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    root.root,
                    0,
                    0,
                    root.width_in_pixels,
                    root.height_in_pixels,
                    u32::MAX,
                )
                .map_err(|error| error.to_string())?
                .reply()
                .map_err(|error| error.to_string())?
                .data;
            let pixels = data
                .chunks_exact(4)
                .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 255])
                .collect();
            return Ok((width, height, pixels));
        }
        #[allow(unreachable_code)]
        Err("screen capture is unavailable on this platform".into())
    }
}

fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|value| value.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
}

fn input_error_reason() -> String {
    if cfg!(target_os = "macos") {
        "local input unavailable: grant Accessibility permission in System Settings > Privacy & Security > Accessibility".into()
    } else if cfg!(target_os = "windows") {
        "local input unavailable: Windows rejected SendInput for this desktop or integrity level"
            .into()
    } else {
        "local input unavailable: could not initialize the X11 input backend".into()
    }
}

fn capture_error(error: String) -> String {
    if cfg!(target_os = "macos") && error.to_ascii_lowercase().contains("permission") {
        "Screen Recording permission is not granted; open System Settings > Privacy & Security > Screen Recording and allow OPCOS".into()
    } else {
        format!("local screen capture unavailable: {error}")
    }
}

fn parse_key(value: &str) -> Result<Key, String> {
    if value.chars().count() == 1 {
        return Ok(Key::Unicode(value.chars().next().unwrap()));
    }
    match value.to_ascii_lowercase().as_str() {
        "enter" | "return" => Ok(Key::Return),
        "tab" => Ok(Key::Tab),
        "escape" | "esc" => Ok(Key::Escape),
        "backspace" => Ok(Key::Backspace),
        "space" => Ok(Key::Space),
        "shift" => Ok(Key::Shift),
        "control" | "ctrl" => Ok(Key::Control),
        "alt" => Ok(Key::Alt),
        "meta" | "command" | "win" => Ok(Key::Meta),
        "up" => Ok(Key::UpArrow),
        "down" => Ok(Key::DownArrow),
        "left" => Ok(Key::LeftArrow),
        "right" => Ok(Key::RightArrow),
        _ => Err(format!("unsupported key name: {value}")),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn x11_capture_and_input_are_real_when_display_is_available() {
        if std::env::var_os("DISPLAY").is_none() {
            return;
        }
        let desktop = LocalDesktop::new();
        let screenshot = desktop.screenshot().expect("X11 screenshot should work");
        let (bounds, pixels) = screenshot.decoded_rgba().expect("PNG should decode");
        assert!(bounds.width > 0 && bounds.height > 0);
        assert_eq!(pixels.len(), (bounds.width * bounds.height * 4) as usize);
        let response = desktop
            .computer_use(ComputerUseAction::MouseMove { coordinate: [0, 0] }, bounds)
            .expect("X11 input should work");
        assert!(response.ok);
    }
}
