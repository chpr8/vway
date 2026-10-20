//! Input injection through the seat — the same path libinput events take (design 0014 §3).

use std::time::Instant;

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState, TouchSlot};
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::input::touch::{DownEvent, MotionEvent as TouchMotionEvent, UpEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use vway_proto::{ButtonAction, ErrorBody, ErrorCode, KeyAction, TouchEvent, TouchPhase, err};

use crate::keymap::Stroke;
use crate::state::State;

/// One wheel detent in `wl_pointer.axis` units (what libinput reports for a mouse wheel click).
const DETENT: f64 = 15.0;

impl State {
    fn touched_input(&mut self) {
        self.last_input = Instant::now();
    }

    fn check_point(&self, x: f64, y: f64) -> Result<Point<f64, Logical>, ErrorBody> {
        if !x.is_finite() || !y.is_finite() {
            return Err(err(ErrorCode::BadRequest, "coordinates must be finite"));
        }
        let size = self.output_size();
        Ok((
            x.clamp(0.0, size.w as f64 - 0.01),
            y.clamp(0.0, size.h as f64 - 0.01),
        )
            .into())
    }

    pub fn pointer_move(&mut self, x: f64, y: f64) -> Result<Point<f64, Logical>, ErrorBody> {
        let pos = self.check_point(x, y)?;
        self.touched_input();
        self.pointer = pos;
        let under = self.surface_under(pos);
        let ptr = self.seat.get_pointer().expect("seat has a pointer");
        let event = MotionEvent {
            location: pos,
            serial: SERIAL_COUNTER.next_serial(),
            time: self.now_ms(),
        };
        ptr.motion(self, under, &event);
        ptr.frame(self);
        Ok(pos)
    }

    pub fn pointer_button(&mut self, button: u32, action: ButtonAction, count: u32) {
        self.touched_input();
        let ptr = self.seat.get_pointer().expect("seat has a pointer");
        let send = |state: &mut State, pressed: bool| {
            if pressed && !ptr.is_grabbed() {
                // Click-to-focus, unless a popup grab owns the pointer.
                let w = state.window_under(state.pointer);
                if let Some(w) = w
                    && state.focused_window().as_ref() != Some(&w)
                {
                    state.focus_window(Some(&w));
                }
            }
            let event = ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: state.now_ms(),
                button,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            };
            ptr.button(state, &event);
            ptr.frame(state);
        };
        match action {
            ButtonAction::Down => send(self, true),
            ButtonAction::Up => send(self, false),
            ButtonAction::Click => {
                for _ in 0..count.max(1) {
                    send(self, true);
                    send(self, false);
                }
            }
        }
    }

    /// `dx`/`dy` in wheel detents; positive `dy` scrolls down, positive `dx` right.
    pub fn scroll(&mut self, dx: f64, dy: f64) -> Result<(), ErrorBody> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err(err(ErrorCode::BadRequest, "scroll amounts must be finite"));
        }
        self.touched_input();
        let mut frame = AxisFrame::new(self.now_ms()).source(AxisSource::Wheel);
        if dy != 0.0 {
            frame = frame
                .value(Axis::Vertical, dy * DETENT)
                .v120(Axis::Vertical, (dy * 120.0).round() as i32);
        }
        if dx != 0.0 {
            frame = frame
                .value(Axis::Horizontal, dx * DETENT)
                .v120(Axis::Horizontal, (dx * 120.0).round() as i32);
        }
        let ptr = self.seat.get_pointer().expect("seat has a pointer");
        ptr.axis(self, frame);
        ptr.frame(self);
        Ok(())
    }

    fn key_event(&mut self, code: Keycode, pressed: bool) {
        let kb = self.seat.get_keyboard().expect("seat has a keyboard");
        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        let time = self.now_ms();
        kb.input::<(), _>(
            self,
            code,
            state,
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::Forward,
        );
    }

    pub fn key_stroke(&mut self, stroke: &Stroke, action: KeyAction, repeat: u32) {
        self.touched_input();
        match action {
            KeyAction::Down => {
                for m in &stroke.mods {
                    self.key_event(*m, true);
                }
                self.key_event(stroke.key, true);
            }
            KeyAction::Up => {
                self.key_event(stroke.key, false);
                for m in stroke.mods.iter().rev() {
                    self.key_event(*m, false);
                }
            }
            KeyAction::Press => {
                for _ in 0..repeat.max(1) {
                    for m in &stroke.mods {
                        self.key_event(*m, true);
                    }
                    self.key_event(stroke.key, true);
                    self.key_event(stroke.key, false);
                    for m in stroke.mods.iter().rev() {
                        self.key_event(*m, false);
                    }
                }
            }
        }
    }

    pub fn type_text(&mut self, text: &str) -> Result<usize, ErrorBody> {
        let strokes = self.keymap.text_strokes(text)?;
        for s in &strokes {
            self.key_stroke(s, KeyAction::Press, 1);
        }
        Ok(strokes.len())
    }

    /// A batch of touch events followed by one `wl_touch.frame`.
    pub fn touch(&mut self, events: &[TouchEvent]) -> Result<(), ErrorBody> {
        // Validate everything first so a bad event does not leave fingers half-down.
        let mut points = Vec::with_capacity(events.len());
        for e in events {
            let p = match (e.phase, e.x, e.y) {
                (TouchPhase::Down | TouchPhase::Move, Some(x), Some(y)) => {
                    Some(self.check_point(x, y)?)
                }
                (TouchPhase::Down | TouchPhase::Move, _, _) => {
                    return Err(err(
                        ErrorCode::BadRequest,
                        format!("touch {:?} needs x and y", e.phase),
                    ));
                }
                _ => None,
            };
            points.push(p);
        }
        if events.is_empty() {
            return Err(err(ErrorCode::BadRequest, "no touch events"));
        }
        // Simulate the batch against the fingers already down.
        let mut down = self.touch_points.clone();
        for e in events {
            let ok = match e.phase {
                TouchPhase::Down => down.insert(e.id),
                TouchPhase::Move => down.contains(&e.id),
                TouchPhase::Up => down.remove(&e.id),
                TouchPhase::Cancel => {
                    down.clear();
                    true
                }
            };
            if !ok {
                let why = if e.phase == TouchPhase::Down {
                    "is already down"
                } else {
                    "is not down"
                };
                return Err(err(
                    ErrorCode::BadRequest,
                    format!("touch {:?}: finger {} {why}", e.phase, e.id),
                ));
            }
        }
        self.touch_points = down;
        self.touched_input();
        let touch = self.seat.get_touch().expect("seat has touch");
        for (e, p) in events.iter().zip(points) {
            let slot = TouchSlot::from(Some(e.id));
            let time = self.now_ms();
            match e.phase {
                TouchPhase::Down => {
                    let pos = p.expect("validated");
                    if let Some(w) = self.window_under(pos)
                        && self.focused_window().as_ref() != Some(&w)
                    {
                        self.focus_window(Some(&w));
                    }
                    let under = self.surface_under(pos);
                    touch.down(
                        self,
                        under,
                        &DownEvent {
                            slot,
                            location: pos,
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    );
                }
                TouchPhase::Move => {
                    let pos = p.expect("validated");
                    let under = self.surface_under(pos);
                    touch.motion(
                        self,
                        under,
                        &TouchMotionEvent {
                            slot,
                            location: pos,
                            time,
                        },
                    );
                }
                TouchPhase::Up => touch.up(
                    self,
                    &UpEvent {
                        slot,
                        serial: SERIAL_COUNTER.next_serial(),
                        time,
                    },
                ),
                TouchPhase::Cancel => touch.cancel(self),
            }
        }
        touch.frame(self);
        Ok(())
    }
}
