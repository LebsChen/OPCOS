use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ScreenBounds {
    pub width: u32,
    pub height: u32,
}

impl ScreenBounds {
    pub fn validate_coordinate(&self, coordinate: [i32; 2]) -> Result<(), ComputerUseError> {
        if self.width == 0
            || self.height == 0
            || coordinate[0] < 0
            || coordinate[1] < 0
            || coordinate[0] as u32 >= self.width
            || coordinate[1] as u32 >= self.height
        {
            return Err(ComputerUseError::InvalidAction(format!(
                "coordinate [{}, {}] is outside {}x{} screen bounds",
                coordinate[0], coordinate[1], self.width, self.height
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ComputerUseError {
    #[error("invalid computer-use action: {0}")]
    InvalidAction(String),
    #[error("invalid screenshot: {0}")]
    InvalidScreenshot(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Screenshot {
    pub image: String,
    #[serde(default = "default_png_format")]
    pub format: String,
}

impl Screenshot {
    pub fn decoded_rgba(&self) -> Result<(ScreenBounds, Vec<u8>), ComputerUseError> {
        let bytes = BASE64.decode(&self.image).map_err(|error| {
            ComputerUseError::InvalidScreenshot(format!("base64 is invalid: {error}"))
        })?;
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().map_err(|error| {
            ComputerUseError::InvalidScreenshot(format!("PNG is invalid: {error}"))
        })?;
        let mut buffer = vec![0; reader.output_buffer_size()];
        let output = reader.next_frame(&mut buffer).map_err(|error| {
            ComputerUseError::InvalidScreenshot(format!("PNG frame is invalid: {error}"))
        })?;
        let pixels = match output.color_type {
            png::ColorType::Rgba => buffer[..output.buffer_size()].to_vec(),
            png::ColorType::Rgb => buffer[..output.buffer_size()]
                .chunks_exact(3)
                .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
                .collect(),
            png::ColorType::Grayscale => buffer[..output.buffer_size()]
                .iter()
                .flat_map(|value| [*value, *value, *value, 255])
                .collect(),
            png::ColorType::GrayscaleAlpha => buffer[..output.buffer_size()]
                .chunks_exact(2)
                .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
                .collect(),
            png::ColorType::Indexed => {
                return Err(ComputerUseError::InvalidScreenshot(
                    "indexed screenshots are unsupported".into(),
                ));
            }
        };
        Ok((
            ScreenBounds {
                width: output.width,
                height: output.height,
            },
            pixels,
        ))
    }

    pub fn dimensions(&self) -> Result<ScreenBounds, ComputerUseError> {
        self.decoded_rgba().map(|(bounds, _)| bounds)
    }
}

fn default_png_format() -> String {
    "png".into()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ComputerUseAction {
    Screenshot,
    CursorPosition,
    Wait,
    Key {
        key: String,
    },
    Type {
        text: String,
    },
    MouseMove {
        coordinate: [i32; 2],
    },
    Scroll {
        coordinate: [i32; 2],
        direction: String,
        amount: i32,
    },
    LeftClick {
        coordinate: [i32; 2],
    },
    RightClick {
        coordinate: [i32; 2],
    },
    MiddleClick {
        coordinate: [i32; 2],
    },
    DoubleClick {
        coordinate: [i32; 2],
    },
    TripleClick {
        coordinate: [i32; 2],
    },
    LeftClickDrag {
        coordinate: [i32; 2],
        #[serde(rename = "coordinate2")]
        coordinate_end: [i32; 2],
    },
    LeftMouseDown {
        coordinate: [i32; 2],
    },
    LeftMouseUp {
        coordinate: [i32; 2],
    },
    HoldKey {
        key: String,
    },
}

impl ComputerUseAction {
    pub fn validate(&self, bounds: ScreenBounds) -> Result<(), ComputerUseError> {
        let coord_check = |coordinate: [i32; 2]| bounds.validate_coordinate(coordinate);
        let text = |value: &str, field: &str| {
            if value.trim().is_empty() {
                return Err(ComputerUseError::InvalidAction(format!(
                    "{field} cannot be empty"
                )));
            }
            if value.chars().count() > 16_384 {
                return Err(ComputerUseError::InvalidAction(format!(
                    "{field} exceeds 16384 characters"
                )));
            }
            Ok(())
        };
        match self {
            Self::Screenshot | Self::CursorPosition | Self::Wait => Ok(()),
            Self::Key { key } | Self::HoldKey { key } => text(key, "key"),
            Self::Type { text: value } => text(value, "text"),
            Self::MouseMove { coordinate }
            | Self::LeftClick { coordinate }
            | Self::RightClick { coordinate }
            | Self::MiddleClick { coordinate }
            | Self::DoubleClick { coordinate }
            | Self::TripleClick { coordinate }
            | Self::LeftMouseDown { coordinate }
            | Self::LeftMouseUp { coordinate } => coord_check(*coordinate),
            Self::Scroll {
                coordinate,
                direction,
                amount,
            } => {
                coord_check(*coordinate)?;
                if !matches!(direction.as_str(), "up" | "down" | "left" | "right") {
                    return Err(ComputerUseError::InvalidAction(
                        "scroll direction must be up, down, left, or right".into(),
                    ));
                }
                if *amount <= 0 || *amount > 10_000 {
                    return Err(ComputerUseError::InvalidAction(
                        "scroll amount must be between 1 and 10000".into(),
                    ));
                }
                Ok(())
            }
            Self::LeftClickDrag {
                coordinate,
                coordinate_end,
            } => {
                coord_check(*coordinate)?;
                coord_check(*coordinate_end)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ComputerUseResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub coordinate: Option<[i32; 2]>,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub error: Option<String>,
}
