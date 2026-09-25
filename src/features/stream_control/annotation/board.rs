//! An opaque Draw stroke below regular ink, scoped to one existing screen.
use super::*;
mod branding;

const BRUSH_WIDTH: f32 = 64.;

#[derive(Clone)]
pub(super) struct Board {
    stroke: Stroke,
    logical_height: f32,
    metrics: Metrics,
    marks: Vec<Stroke>,
}
pub(super) struct BoardEdit {
    screen: i32,
    value: Option<Board>,
    remaining: usize,
}

pub(super) fn complete(a: &mut Annotation) {
    let Some(edit) = &mut a.board_edit else {
        return;
    };
    edit.remaining = edit.remaining.saturating_sub(1);
    if edit.remaining == 0 {
        let edit = a.board_edit.take().unwrap();
        if let Some(board) = edit.value {
            a.boards.insert(edit.screen, board);
        } else {
            a.boards.remove(&edit.screen);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Metrics {
    width: u32,
    height: u32,
    dpi: u32,
}
impl Metrics {
    fn from_display(display: &ScreenBaseline) -> Self {
        Self {
            width: display.pixel_width.max(display.width).max(1),
            height: display.pixel_height.max(display.height).max(1),
            dpi: if display.display.current_dpi == 0 {
                100
            } else {
                display.display.current_dpi
            },
        }
    }
    fn logical_height(self) -> f32 {
        self.height as f32 * 100. / self.dpi as f32
    }
}

fn fill(id: u32, screen: i32, metrics: Metrics, rgb: [u8; 3]) -> Result<Board> {
    let height = metrics.logical_height();
    let rows = (height / (BRUSH_WIDTH * 0.5)).ceil().max(1.) as usize;
    if rows > MAX_POINTS / 6 - 1 {
        bail!("当前屏幕尺寸超出白板绘制范围");
    }
    let mut points = Vec::with_capacity((rows + 1) * 6);
    for row in 0..=rows {
        let y = row as f32 / rows as f32;
        let (left, right) = if row % 2 == 0 { (0., 1.) } else { (1., 0.) };
        // Duplicate turns preserve coverage at the edges under host Bezier smoothing.
        points.extend([Point { x: left, y }; 3]);
        points.extend([Point { x: right, y }; 3]);
    }
    Ok(Board {
        stroke: Stroke {
            id,
            screen,
            points,
            style: Style {
                argb: u32::from_be_bytes([255, rgb[0], rgb[1], rgb[2]]),
                width: BRUSH_WIDTH,
            },
        },
        logical_height: height,
        metrics,
        marks: Vec::new(),
    })
}

impl StreamControlHandle {
    pub(crate) fn annotation_board_color(&self, screen: i32) -> Option<[u8; 3]> {
        lock(&self.shared)
            .annotation
            .boards
            .get(&screen)
            .map(|board| {
                let [_, r, g, b] = board.stroke.style.argb.to_be_bytes();
                [r, g, b]
            })
    }

    pub(crate) fn annotation_board(
        &self,
        owner: u64,
        screen: i32,
        color: Option<[u8; 3]>,
    ) -> Result<()> {
        let mut s = lock(&self.shared);
        ensure_ready(&s)?;
        if !s.annotation.owners.contains(&owner) {
            bail!("批注窗口已关闭");
        }
        self.set_board_locked(&mut s, screen, color)
    }

    fn set_board_locked(
        &self,
        s: &mut StreamControlState,
        screen: i32,
        color: Option<[u8; 3]>,
    ) -> Result<()> {
        if !s.annotation.enabled || s.annotation.toggling() || s.annotation.uncertain {
            bail!("批注尚未就绪");
        }
        if s.annotation.busy() {
            bail!("请等待当前批注操作完成");
        }
        let display = s
            .screens
            .iter()
            .find(|v| v.id == screen && screen >= 0)
            .ok_or_else(|| anyhow!("批注屏幕已变化"))?;
        let metrics = Metrics::from_display(display);
        let height = metrics.logical_height();
        let old = s.annotation.boards.get(&screen);
        let mut requests = Vec::new();
        let mut value = if let Some(rgb) = color {
            let id = if let Some(board) = old {
                board.stroke.id
            } else {
                (1..=BOARD_IDS)
                    .step_by(BOARD_SLOT_IDS as usize)
                    .find(|id| !s.annotation.boards.values().any(|b| b.stroke.id == *id))
                    .ok_or_else(|| anyhow!("白板数量已达到上限"))?
            };
            let argb = u32::from_be_bytes([255, rgb[0], rgb[1], rgb[2]]);
            if let Some(old) = old.filter(|board| height <= board.logical_height) {
                if old.stroke.style.argb == argb && old.metrics == metrics {
                    return Ok(());
                }
                let mut board = old.clone();
                board.stroke.style.argb = argb;
                board.metrics = metrics;
                if old.stroke.style.argb != argb {
                    requests.push(stroke_request(
                        &board.stroke,
                        &[*board.stroke.points.last().unwrap()],
                    ));
                }
                Some(board)
            } else {
                let board = fill(id, screen, metrics, rgb)?;
                if old.is_some() {
                    requests.push(clear_request(2, id, Some(screen)));
                }
                requests.extend(
                    board
                        .stroke
                        .points
                        .chunks(4096)
                        .map(|points| stroke_request(&board.stroke, points)),
                );
                Some(board)
            }
        } else {
            let Some(old) = old else {
                return Ok(());
            };
            requests.push(clear_request(2, old.stroke.id, Some(screen)));
            None
        };
        // All logo/text strokes share this screen's reserved background slot.
        // Remove old lettering before changing the backdrop, then draw the new header.
        let old_marks = old
            .map(|board| {
                board
                    .marks
                    .iter()
                    .map(|mark| clear_request(2, mark.id, Some(screen)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        requests.splice(0..0, old_marks);
        if let Some(board) = &mut value {
            let [_, r, g, b] = board.stroke.style.argb.to_be_bytes();
            board.marks = branding::build(board.stroke.id, screen, metrics, [r, g, b])?;
            requests.extend(board.marks.iter().flat_map(|mark| {
                mark.points
                    .chunks(4096)
                    .map(|points| stroke_request(mark, points))
            }));
        }
        if requests.len() > MAX_PENDING {
            bail!("白板标识请求超出范围");
        }
        s.annotation.board_edit = Some(BoardEdit {
            screen,
            value,
            remaining: requests.len(),
        });
        s.annotation.error = None;
        for request in requests {
            let kind = if matches!(request.payload, Some(PbDrawRequestKind::Stroke(_))) {
                1
            } else {
                2
            };
            if let Err(e) = self.send_draw(s, request, Pending::Board(kind)) {
                s.annotation.uncertain(e.to_string());
                return Err(e);
            }
        }
        Ok(())
    }

    pub(super) fn refresh_board(&self, s: &mut StreamControlState) {
        if !s.annotation.enabled || s.annotation.uncertain || s.annotation.busy() {
            return;
        }
        s.annotation
            .boards
            .retain(|screen, _| s.screens.iter().any(|v| v.id == *screen));
        let changed = s.annotation.boards.iter().find_map(|(screen, board)| {
            s.screens
                .iter()
                .find(|v| v.id == *screen)
                .filter(|v| Metrics::from_display(v) != board.metrics)
                .map(|_| {
                    let [_, r, g, b] = board.stroke.style.argb.to_be_bytes();
                    (*screen, [r, g, b])
                })
        });
        if let Some((screen, color)) = changed
            && let Err(e) = self.set_board_locked(s, screen, Some(color))
        {
            s.annotation.uncertain(e.to_string());
        }
    }
}
