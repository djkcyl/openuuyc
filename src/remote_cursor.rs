//! Remote pointer state, independent of video decoding and local input.
//! GameViewerServer 589240: Message(15).SystemStateChange(2).CursorShape.
//! pos_x/y are image hotspots; coordinate_*_scale are sampled at shape changes,
//! not continuously transmitted. Watching uses capture-side cursor composition.

use std::sync::{Arc, Mutex};

use anyhow::{Result, ensure};
use prost::Message;

const MAX_PNG_BYTES: usize = 4 * 1024 * 1024;
const MAX_CURSOR_SIZE: u32 = 2048;

#[derive(Clone, Debug, PartialEq)]
pub struct CursorImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub hotspot: [u32; 2],
    pub system_type: i32,
}

#[derive(Clone, Debug)]
pub struct RemoteCursor {
    pub image: Arc<CursorImage>,
    /// Position sampled with this shape, not a live pointer position.
    pub sampled_position: Option<[f64; 2]>,
    pub screen_id: i32,
}

#[derive(Clone, Default)]
pub(crate) struct RemoteCursorState(Arc<Mutex<CursorState>>);

#[derive(Default)]
struct CursorState {
    cursor: Option<RemoteCursor>,
    hidden: bool,
}

impl RemoteCursorState {
    pub fn snapshot(&self) -> Option<RemoteCursor> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cursor
            .clone()
    }

    pub fn hidden(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).hidden
    }

    pub fn clear(&self) {
        self.publish(None, false);
    }

    fn publish(&self, mut cursor: Option<RemoteCursor>, hidden: bool) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let (Some(previous), Some(next)) = (state.cursor.as_ref(), &mut cursor)
            && previous.image == next.image
        {
            next.image = previous.image.clone();
        }
        state.cursor = cursor;
        state.hidden = hidden;
    }

    pub fn receive(&self, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_PNG_BYTES + 1024,
            "cursor state message too large"
        );
        let state = SystemStateChange::decode(bytes)?;
        let Some(SystemState::Cursor(shape)) = state.state else {
            return Ok(());
        };
        tracing::trace!(
            screen = shape.screen_id,
            kind = shape.cursor_type,
            width = shape.width,
            height = shape.height,
            bytes = shape.byte_value.len(),
            x = shape.coordinate_x_scale,
            y = shape.coordinate_y_scale,
            "remote cursor state"
        );
        let hidden = shape.cursor_type == -1;
        let parsed = parse_shape(shape);
        match parsed {
            Ok(cursor) => self.publish(cursor, hidden),
            Err(error) => {
                self.clear();
                return Err(error);
            }
        }
        Ok(())
    }
}

fn parse_shape(shape: CursorShape) -> Result<Option<RemoteCursor>> {
    // The official sender publishes type -1 with no image when hidden.
    if shape.cursor_type == -1 {
        return Ok(None);
    }
    let position = [shape.coordinate_x_scale, shape.coordinate_y_scale];
    let sampled_position = (shape.screen_id >= 0
        && position
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v)))
    .then_some(position);
    ensure!(
        shape.byte_value.len() <= MAX_PNG_BYTES,
        "cursor PNG too large"
    );
    ensure!(
        shape.width >= 0 && shape.height >= 0,
        "negative cursor dimensions"
    );
    let (width, height) = (shape.width as u32, shape.height as u32);
    ensure!(
        width <= MAX_CURSOR_SIZE && height <= MAX_CURSOR_SIZE,
        "cursor dimensions too large"
    );
    if !shape.byte_value.is_empty() {
        ensure!(width > 0 && height > 0, "empty cursor dimensions");
        ensure!(
            shape.pos_x >= 0
                && shape.pos_y >= 0
                && shape.pos_x < shape.width
                && shape.pos_y < shape.height,
            "invalid cursor hotspot"
        );
    }
    Ok(Some(RemoteCursor {
        image: Arc::new(CursorImage {
            png: shape.byte_value,
            width,
            height,
            hotspot: [shape.pos_x.max(0) as u32, shape.pos_y.max(0) as u32],
            system_type: shape.cursor_type,
        }),
        sampled_position,
        screen_id: shape.screen_id,
    }))
}

#[derive(Clone, PartialEq, Message)]
struct SystemStateChange {
    #[prost(oneof = "SystemState", tags = "1, 2, 3, 4, 5")]
    state: Option<SystemState>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
enum SystemState {
    #[prost(bytes, tag = "1")]
    SecureDesktop(Vec<u8>),
    #[prost(message, tag = "2")]
    Cursor(CursorShape),
    #[prost(bytes, tag = "3")]
    Permission(Vec<u8>),
    #[prost(bytes, tag = "4")]
    FileTransfer(Vec<u8>),
    #[prost(bytes, tag = "5")]
    PrivateScreen(Vec<u8>),
}

#[derive(Clone, PartialEq, Message)]
struct CursorShape {
    #[prost(int32, tag = "1")]
    pos_x: i32,
    #[prost(int32, tag = "2")]
    pos_y: i32,
    #[prost(int32, tag = "3")]
    width: i32,
    #[prost(int32, tag = "4")]
    height: i32,
    #[prost(bytes, tag = "5")]
    byte_value: Vec<u8>,
    #[prost(int32, tag = "6")]
    cursor_type: i32,
    #[prost(double, tag = "7")]
    coordinate_x_scale: f64,
    #[prost(double, tag = "8")]
    coordinate_y_scale: f64,
    #[prost(int32, tag = "9")]
    screen_id: i32,
}
