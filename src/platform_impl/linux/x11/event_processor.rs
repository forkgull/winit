use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::{mpsc, Arc, Mutex},
};

use x11rb::protocol::{
    xinput, xkb,
    xproto::{self, ConnectionExt as _},
    Event as X11Event,
};
use x11rb::{connection::Connection, x11_utils::Serialize};

use super::{
    atoms::*, get_xtarget, ime, mkdid, mkwid, util, xinput_fp1616_to_float, xinput_fp3232_to_float,
    CookieResultExt, Device, DeviceId, DeviceInfo, Dnd, DndState, ScrollOrientation, UnownedWindow,
    WindowId,
};

use crate::event::InnerSizeWriter;
use crate::event::{
    ElementState::{Pressed, Released},
    MouseButton::{Back, Forward, Left, Middle, Other, Right},
    MouseScrollDelta::LineDelta,
    Touch,
    WindowEvent::{
        AxisMotion, CursorEntered, CursorLeft, CursorMoved, Focused, MouseInput, MouseWheel,
    },
};
use crate::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{DeviceEvent, ElementState, Event, Ime, RawKeyEvent, TouchPhase, WindowEvent},
    event_loop::EventLoopWindowTarget as RootELW,
    keyboard::ModifiersState,
    platform_impl::platform::common::{keymap, xkb_state::KbdState},
};

/// The X11 documentation states: "Keycodes lie in the inclusive range `[8, 255]`".
const KEYCODE_OFFSET: u8 = 8;

pub(super) struct EventProcessor<T: 'static> {
    /// Queue of unprocessed events.
    pub(super) event_queue: Rc<RefCell<VecDeque<X11Event>>>,

    pub(super) dnd: Dnd,
    /// Requests from other threads for IME.
    pub(super) ime_requests: mpsc::Receiver<ime::ImeRequest>,
    pub(super) devices: RefCell<HashMap<DeviceId, Device>>,
    pub(super) target: Rc<RootELW<T>>,
    pub(super) kb_state: KbdState,
    // Number of touch events currently in progress
    pub(super) num_touch: u32,
    // This is the last pressed key that is repeatable (if it hasn't been
    // released).
    //
    // Used to detect key repeats.
    pub(super) held_key_press: Option<u32>,
    pub(super) first_touch: Option<u64>,
    // Currently focused window belonging to this process
    pub(super) active_window: Option<xproto::Window>,
    pub(super) is_composing: bool,
}

impl<T: 'static> EventProcessor<T> {
    pub(super) fn init_device(&self, device: xinput::DeviceId) {
        let wt = get_xtarget(&self.target);
        let mut devices = self.devices.borrow_mut();
        if let Some(info) = DeviceInfo::get(&wt.xconn, device as _) {
            for info in info.iter() {
                devices.insert(DeviceId(info.deviceid as _), Device::new(info));
            }
        }
    }

    pub(crate) fn with_window<F, Ret>(&self, window_id: xproto::Window, callback: F) -> Option<Ret>
    where
        F: Fn(&Arc<UnownedWindow>) -> Ret,
    {
        let mut deleted = false;
        let window_id = WindowId(window_id as _);
        let wt = get_xtarget(&self.target);
        let result = wt
            .windows
            .borrow()
            .get(&window_id)
            .and_then(|window| {
                let arc = window.upgrade();
                deleted = arc.is_none();
                arc
            })
            .map(|window| callback(&window));
        if deleted {
            // Garbage collection
            wt.windows.borrow_mut().remove(&window_id);
        }
        result
    }

    fn window_exists(&self, window_id: xproto::Window) -> bool {
        self.with_window(window_id, |_| ()).is_some()
    }

    pub(super) fn poll(&self) -> bool {
        // See if we have a cached event.
        if !self.event_queue.borrow().is_empty() {
            return true;
        }

        // If not, try to poll for one,
        match get_xtarget(&self.target)
            .x_connection()
            .xcb_connection()
            .poll_for_event()
            .expect("Error while polling for X11 events")
        {
            None => false,
            Some(event) => {
                self.event_queue.borrow_mut().push_back(event);
                true
            }
        }
    }

    pub(super) fn poll_one_event(&mut self) -> Option<X11Event> {
        if let Some(event) = self.event_queue.borrow_mut().pop_front() {
            return Some(event);
        }

        get_xtarget(&self.target)
            .x_connection()
            .xcb_connection()
            .poll_for_event()
            .expect("Error while polling for X11 events")
    }

    pub(super) fn process_event<F>(&mut self, event: X11Event, mut callback: F)
    where
        F: FnMut(Event<T>),
    {
        let wt = get_xtarget(&self.target);
        let atoms = wt.x_connection().atoms();

        match &event {
            X11Event::ClientMessage(client_msg) => {
                let window = client_msg.window as xproto::Window;
                let window_id = mkwid(window);

                let data = client_msg.data.as_data32();
                if data[0] == wt.wm_delete_window {
                    callback(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::CloseRequested,
                    });
                } else if data[0] == wt.net_wm_ping {
                    wt.xconn
                        .xcb_connection()
                        .send_event(
                            false,
                            wt.root,
                            xproto::EventMask::SUBSTRUCTURE_NOTIFY
                                | xproto::EventMask::SUBSTRUCTURE_REDIRECT,
                            client_msg.serialize(),
                        )
                        .expect_then_ignore_error("Failed to send `ClientMessage` event.");
                } else if client_msg.type_ == atoms[XdndEnter] {
                    let source_window = data[0];
                    let flags = data[1];
                    let version = flags >> 24;
                    self.dnd.version = Some(version);
                    let has_more_types = flags - (flags & (u32::max_value() - 1)) == 1;
                    if !has_more_types {
                        let type_list = vec![data[2], data[3], data[4]];
                        self.dnd.type_list = Some(type_list);
                    } else if let Ok(more_types) = unsafe { self.dnd.get_type_list(source_window) }
                    {
                        self.dnd.type_list = Some(more_types);
                    }
                } else if client_msg.type_ == atoms[XdndPosition] {
                    // This event occurs every time the mouse moves while a file's being dragged
                    // over our window. We emit HoveredFile in response; while the macOS backend
                    // does that upon a drag entering, XDND doesn't have access to the actual drop
                    // data until this event. For parity with other platforms, we only emit
                    // `HoveredFile` the first time, though if winit's API is later extended to
                    // supply position updates with `HoveredFile` or another event, implementing
                    // that here would be trivial.

                    let source_window = data[0];

                    // Equivalent to `(x << shift) | y`
                    // where `shift = mem::size_of::<c_short>() * 8`
                    // Note that coordinates are in "desktop space", not "window space"
                    // (in X11 parlance, they're root window coordinates)
                    //let packed_coordinates = client_msg.data.get_long(2);
                    //let shift = mem::size_of::<libc::c_short>() * 8;
                    //let x = packed_coordinates >> shift;
                    //let y = packed_coordinates & !(x << shift);

                    // By our own state flow, `version` should never be `None` at this point.
                    let version = self.dnd.version.unwrap_or(5);

                    // Action is specified in versions 2 and up, though we don't need it anyway.
                    //let action = client_msg.data.get_long(4);

                    let accepted = if let Some(ref type_list) = self.dnd.type_list {
                        type_list.contains(&atoms[TextUriList])
                    } else {
                        false
                    };

                    if accepted {
                        self.dnd.source_window = Some(source_window);
                        unsafe {
                            if self.dnd.result.is_none() {
                                let time = if version >= 1 {
                                    data[3] as xproto::Timestamp
                                } else {
                                    // In version 0, time isn't specified
                                    x11rb::CURRENT_TIME
                                };

                                // Log this timestamp.
                                wt.xconn.set_timestamp(time);

                                // This results in the `SelectionNotify` event below
                                self.dnd.convert_selection(window, time);
                            }
                            self.dnd
                                .send_status(window, source_window, DndState::Accepted)
                                .expect("Failed to send `XdndStatus` message.");
                        }
                    } else {
                        unsafe {
                            self.dnd
                                .send_status(window, source_window, DndState::Rejected)
                                .expect("Failed to send `XdndStatus` message.");
                        }
                        self.dnd.reset();
                    }
                } else if client_msg.type_ == atoms[XdndDrop] {
                    let (source_window, state) = if let Some(source_window) = self.dnd.source_window
                    {
                        if let Some(Ok(ref path_list)) = self.dnd.result {
                            for path in path_list {
                                callback(Event::WindowEvent {
                                    window_id,
                                    event: WindowEvent::DroppedFile(path.clone()),
                                });
                            }
                        }
                        (source_window, DndState::Accepted)
                    } else {
                        // `source_window` won't be part of our DND state if we already rejected the drop in our
                        // `XdndPosition` handler.
                        let source_window = data[0];
                        (source_window, DndState::Rejected)
                    };
                    unsafe {
                        self.dnd
                            .send_finished(window, source_window, state)
                            .expect("Failed to send `XdndFinished` message.");
                    }
                    self.dnd.reset();
                } else if client_msg.type_ == atoms[XdndLeave] {
                    self.dnd.reset();
                    callback(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::HoveredFileCancelled,
                    });
                }
            }

            X11Event::SelectionNotify(xsel) => {
                let window = xsel.requestor;
                let window_id = mkwid(window);

                // Set the timestamp.
                wt.xconn.set_timestamp(xsel.time as xproto::Timestamp);

                if xsel.property == atoms[XdndSelection] {
                    let mut result = None;

                    // This is where we receive data from drag and drop
                    if let Ok(mut data) = unsafe { self.dnd.read_data(window) } {
                        let parse_result = self.dnd.parse_data(&mut data);
                        if let Ok(ref path_list) = parse_result {
                            for path in path_list {
                                callback(Event::WindowEvent {
                                    window_id,
                                    event: WindowEvent::HoveredFile(path.clone()),
                                });
                            }
                        }
                        result = Some(parse_result);
                    }

                    self.dnd.result = result;
                }
            }

            X11Event::ConfigureNotify(xev) => {
                let xwindow = xev.window;
                let window_id = mkwid(xwindow);

                if let Some(window) = self.with_window(xwindow, Arc::clone) {
                    // So apparently...
                    // `XSendEvent` (synthetic `ConfigureNotify`) -> position relative to root
                    // `XConfigureNotify` (real `ConfigureNotify`) -> position relative to parent
                    // https://tronche.com/gui/x/icccm/sec-4.html#s-4.1.5
                    // We don't want to send `Moved` when this is false, since then every `Resized`
                    // (whether the window moved or not) is accompanied by an extraneous `Moved` event
                    // that has a position relative to the parent window.
                    let is_synthetic = xev.response_type & 0x80 == 0;

                    // These are both in physical space.
                    let new_inner_size = (xev.width as u32, xev.height as u32);
                    let new_inner_position = (xev.x as i32, xev.y as i32);

                    let (mut resized, moved) = {
                        let mut shared_state_lock = window.shared_state_lock();

                        let resized =
                            util::maybe_change(&mut shared_state_lock.size, new_inner_size);
                        let moved = if is_synthetic {
                            util::maybe_change(
                                &mut shared_state_lock.inner_position,
                                new_inner_position,
                            )
                        } else {
                            // Detect when frame extents change.
                            // Since this isn't synthetic, as per the notes above, this position is relative to the
                            // parent window.
                            let rel_parent = new_inner_position;
                            if util::maybe_change(
                                &mut shared_state_lock.inner_position_rel_parent,
                                rel_parent,
                            ) {
                                // This ensures we process the next `Moved`.
                                shared_state_lock.inner_position = None;
                                // Extra insurance against stale frame extents.
                                shared_state_lock.frame_extents = None;
                            }
                            false
                        };
                        (resized, moved)
                    };

                    let position = window.shared_state_lock().position;

                    let new_outer_position = if let (Some(position), false) = (position, moved) {
                        position
                    } else {
                        let mut shared_state_lock = window.shared_state_lock();

                        // We need to convert client area position to window position.
                        let frame_extents = shared_state_lock
                            .frame_extents
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| {
                                let frame_extents =
                                    wt.xconn.get_frame_extents_heuristic(xwindow, wt.root);
                                shared_state_lock.frame_extents = Some(frame_extents.clone());
                                frame_extents
                            });
                        let outer = frame_extents
                            .inner_pos_to_outer(new_inner_position.0, new_inner_position.1);
                        shared_state_lock.position = Some(outer);

                        // Unlock shared state to prevent deadlock in callback below
                        drop(shared_state_lock);

                        if moved {
                            callback(Event::WindowEvent {
                                window_id,
                                event: WindowEvent::Moved(outer.into()),
                            });
                        }
                        outer
                    };

                    if is_synthetic {
                        let mut shared_state_lock = window.shared_state_lock();
                        // If we don't use the existing adjusted value when available, then the user can screw up the
                        // resizing by dragging across monitors *without* dropping the window.
                        let (width, height) = shared_state_lock
                            .dpi_adjusted
                            .unwrap_or((xev.width as u32, xev.height as u32));

                        let last_scale_factor = shared_state_lock.last_monitor.scale_factor;
                        let new_scale_factor = {
                            let window_rect = util::AaRect::new(new_outer_position, new_inner_size);
                            let monitor = wt
                                .xconn
                                .get_monitor_for_window(Some(window_rect))
                                .expect("Failed to find monitor for window");

                            if monitor.is_dummy() {
                                // Avoid updating monitor using a dummy monitor handle
                                last_scale_factor
                            } else {
                                shared_state_lock.last_monitor = monitor.clone();
                                monitor.scale_factor
                            }
                        };
                        if last_scale_factor != new_scale_factor {
                            let (new_width, new_height) = window.adjust_for_dpi(
                                last_scale_factor,
                                new_scale_factor,
                                width,
                                height,
                                &shared_state_lock,
                            );

                            let old_inner_size = PhysicalSize::new(width, height);
                            let new_inner_size = PhysicalSize::new(new_width, new_height);

                            // Unlock shared state to prevent deadlock in callback below
                            drop(shared_state_lock);

                            let inner_size = Arc::new(Mutex::new(new_inner_size));
                            callback(Event::WindowEvent {
                                window_id,
                                event: WindowEvent::ScaleFactorChanged {
                                    scale_factor: new_scale_factor,
                                    inner_size_writer: InnerSizeWriter::new(Arc::downgrade(
                                        &inner_size,
                                    )),
                                },
                            });

                            let new_inner_size = *inner_size.lock().unwrap();
                            drop(inner_size);

                            if new_inner_size != old_inner_size {
                                window.request_inner_size_physical(
                                    new_inner_size.width,
                                    new_inner_size.height,
                                );
                                window.shared_state_lock().dpi_adjusted =
                                    Some(new_inner_size.into());
                                // if the DPI factor changed, force a resize event to ensure the logical
                                // size is computed with the right DPI factor
                                resized = true;
                            }
                        }
                    }

                    let mut shared_state_lock = window.shared_state_lock();

                    // This is a hack to ensure that the DPI adjusted resize is actually applied on all WMs. KWin
                    // doesn't need this, but Xfwm does. The hack should not be run on other WMs, since tiling
                    // WMs constrain the window size, making the resize fail. This would cause an endless stream of
                    // XResizeWindow requests, making Xorg, the winit client, and the WM consume 100% of CPU.
                    if let Some(adjusted_size) = shared_state_lock.dpi_adjusted {
                        if new_inner_size == adjusted_size || !util::wm_name_is_one_of(&["Xfwm4"]) {
                            // When this finally happens, the event will not be synthetic.
                            shared_state_lock.dpi_adjusted = None;
                        } else {
                            window.request_inner_size_physical(adjusted_size.0, adjusted_size.1);
                        }
                    }

                    // Unlock shared state to prevent deadlock in callback below
                    drop(shared_state_lock);

                    if resized {
                        callback(Event::WindowEvent {
                            window_id,
                            event: WindowEvent::Resized(new_inner_size.into()),
                        });
                    }
                }
            }

            X11Event::ReparentNotify(xev) => {
                // This is generally a reliable way to detect when the window manager's been
                // replaced, though this event is only fired by reparenting window managers
                // (which is almost all of them). Failing to correctly update WM info doesn't
                // really have much impact, since on the WMs affected (xmonad, dwm, etc.) the only
                // effect is that we waste some time trying to query unsupported properties.
                wt.xconn.update_cached_wm_info(wt.root);

                self.with_window(xev.window as xproto::Window, |window| {
                    window.invalidate_cached_frame_extents();
                });
            }
            X11Event::MapNotify(xev) => {
                let window = xev.window as xproto::Window;
                let window_id = mkwid(window);

                // XXX re-issue the focus state when mapping the window.
                //
                // The purpose of it is to deliver initial focused state of the newly created
                // window, given that we can't rely on `CreateNotify`, due to it being not
                // sent.
                let focus = self
                    .with_window(window, |window| window.has_focus())
                    .unwrap_or_default();
                callback(Event::WindowEvent {
                    window_id,
                    event: WindowEvent::Focused(focus),
                });
            }
            X11Event::DestroyNotify(xev) => {
                let window = xev.window as xproto::Window;
                let window_id = mkwid(window);

                // In the event that the window's been destroyed without being dropped first, we
                // cleanup again here.
                wt.windows.borrow_mut().remove(&WindowId(window as _));

                // Since all XIM stuff needs to happen from the same thread, we destroy the input
                // context here instead of when dropping the window.
                if let Some(ime) = wt.ime.as_ref() {
                    ime.borrow_mut()
                        .remove_context(window)
                        .expect("Failed to destroy input context");
                }

                callback(Event::WindowEvent {
                    window_id,
                    event: WindowEvent::Destroyed,
                });
            }

            X11Event::VisibilityNotify(xev) => {
                let xwindow = xev.window as xproto::Window;
                callback(Event::WindowEvent {
                    window_id: mkwid(xwindow),
                    event: WindowEvent::Occluded(xev.state == xproto::Visibility::FULLY_OBSCURED),
                });
                self.with_window(xwindow, |window| {
                    window.visibility_notify();
                });
            }

            X11Event::Expose(xev) => {
                // Multiple Expose events may be received for subareas of a window.
                // We issue `RedrawRequested` only for the last event of such a series.
                if xev.count == 0 {
                    let window = xev.window as xproto::Window;
                    let window_id = mkwid(window);

                    callback(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::RedrawRequested,
                    });
                }
            }

            // Note that in compose/pre-edit sequences, we'll always receive KeyRelease events
            X11Event::KeyPress(xkev) | X11Event::KeyRelease(xkev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xkev.time as xproto::Timestamp);

                let window = match self.active_window {
                    Some(window) => window,
                    None => return,
                };

                let window_id = mkwid(window);
                let device_id = mkdid(util::VIRTUAL_CORE_KEYBOARD);

                let keycode = xkev.detail.into();

                // Update state to track key repeats and determine whether this key was a repeat.
                //
                // Note, when a key is held before focusing on this window the first
                // (non-synthetic) event will not be flagged as a repeat (also note that the
                // synthetic press event that is generated before this when the window gains focus
                // will also not be flagged as a repeat).
                //
                // Only keys that can repeat should change the held_key_press state since a
                // continuously held repeatable key may continue repeating after the press of a
                // non-repeatable key.
                let repeat = if self.kb_state.key_repeats(keycode) {
                    let is_latest_held = self.held_key_press == Some(keycode);

                    if matches!(event, X11Event::KeyPress(_)) {
                        self.held_key_press = Some(keycode);
                        is_latest_held
                    } else {
                        // Check that the released key is the latest repeatable key that has been
                        // pressed, since repeats will continue for the latest key press if a
                        // different previously pressed key is released.
                        if is_latest_held {
                            self.held_key_press = None;
                        }
                        false
                    }
                } else {
                    false
                };

                let state = if matches!(event, X11Event::KeyPress(_)) {
                    ElementState::Pressed
                } else {
                    ElementState::Released
                };

                if keycode != 0 && !self.is_composing {
                    let event = self.kb_state.process_key_event(keycode, state, repeat);
                    callback(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::KeyboardInput {
                            device_id,
                            event,
                            is_synthetic: false,
                        },
                    });
                }
            }

            X11Event::XinputButtonPress(xev) | X11Event::XinputButtonRelease(xev) => {
                let window_id = mkwid(xev.event as xproto::Window);
                let device_id = mkdid(xev.deviceid as xinput::DeviceId);

                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                if xev
                    .flags
                    .contains(xinput::PointerEventFlags::POINTER_EMULATED)
                {
                    // Deliver multi-touch events instead of emulated mouse events.
                    return;
                }

                let state = if matches!(event, X11Event::XinputButtonPress(_)) {
                    Pressed
                } else {
                    Released
                };
                match xev.detail {
                    1 => callback(Event::WindowEvent {
                        window_id,
                        event: MouseInput {
                            device_id,
                            state,
                            button: Left,
                        },
                    }),
                    2 => callback(Event::WindowEvent {
                        window_id,
                        event: MouseInput {
                            device_id,
                            state,
                            button: Middle,
                        },
                    }),
                    3 => callback(Event::WindowEvent {
                        window_id,
                        event: MouseInput {
                            device_id,
                            state,
                            button: Right,
                        },
                    }),

                    // Suppress emulated scroll wheel clicks, since we handle the real motion events for those.
                    // In practice, even clicky scroll wheels appear to be reported by evdev (and XInput2 in
                    // turn) as axis motion, so we don't otherwise special-case these button presses.
                    4..=7 => {
                        if xev
                            .flags
                            .contains(xinput::PointerEventFlags::POINTER_EMULATED)
                        {
                            callback(Event::WindowEvent {
                                window_id,
                                event: MouseWheel {
                                    device_id,
                                    delta: match xev.detail {
                                        4 => LineDelta(0.0, 1.0),
                                        5 => LineDelta(0.0, -1.0),
                                        6 => LineDelta(1.0, 0.0),
                                        7 => LineDelta(-1.0, 0.0),
                                        _ => unreachable!(),
                                    },
                                    phase: TouchPhase::Moved,
                                },
                            });
                        }
                    }

                    8 => callback(Event::WindowEvent {
                        window_id,
                        event: MouseInput {
                            device_id,
                            state,
                            button: Back,
                        },
                    }),
                    9 => callback(Event::WindowEvent {
                        window_id,
                        event: MouseInput {
                            device_id,
                            state,
                            button: Forward,
                        },
                    }),

                    x => callback(Event::WindowEvent {
                        window_id,
                        event: MouseInput {
                            device_id,
                            state,
                            button: Other(x as u16),
                        },
                    }),
                }
            }
            X11Event::XinputMotion(xev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                let device_id = mkdid(xev.deviceid as xinput::DeviceId);
                let window = xev.event as xproto::Window;
                let window_id = mkwid(window);
                let new_cursor_pos = (
                    xinput_fp1616_to_float(xev.event_x),
                    xinput_fp1616_to_float(xev.event_y),
                );

                let cursor_moved = self.with_window(window, |window| {
                    let mut shared_state_lock = window.shared_state_lock();
                    util::maybe_change(&mut shared_state_lock.cursor_pos, new_cursor_pos)
                });
                if cursor_moved == Some(true) {
                    callback(Event::WindowEvent {
                        window_id,
                        event: CursorMoved {
                            device_id,
                            position: new_cursor_pos.into(),
                        },
                    });
                } else if cursor_moved.is_none() {
                    return;
                }

                // More gymnastics, for self.devices
                let mut events = Vec::new();
                {
                    let mut devices = self.devices.borrow_mut();
                    let physical_device =
                        match devices.get_mut(&DeviceId(xev.sourceid as xinput::DeviceId)) {
                            Some(device) => device,
                            None => return,
                        };

                    let mask = bytemuck::cast_slice::<u32, u8>(&xev.valuator_mask);
                    let mut values = &*xev.axisvalues;

                    for i in 0..mask.len() * 8 {
                        let byte_index = i / 8;
                        let bit_index = i % 8;

                        if mask[byte_index] & (1 << bit_index) == 0 {
                            continue;
                        }

                        // This mask is set, get the value.
                        let x = {
                            let (value, rest) = values.split_first().unwrap();
                            values = rest;
                            xinput_fp3232_to_float(*value)
                        };

                        if let Some(&mut (_, ref mut info)) = physical_device
                            .scroll_axes
                            .iter_mut()
                            .find(|&&mut (axis, _)| axis == i as _)
                        {
                            let delta = (x - info.position) / info.increment;
                            info.position = x;
                            events.push(Event::WindowEvent {
                                window_id,
                                event: MouseWheel {
                                    device_id,
                                    delta: match info.orientation {
                                        // X11 vertical scroll coordinates are opposite to winit's
                                        ScrollOrientation::Horizontal => {
                                            LineDelta(-delta as f32, 0.0)
                                        }
                                        ScrollOrientation::Vertical => {
                                            LineDelta(0.0, -delta as f32)
                                        }
                                    },
                                    phase: TouchPhase::Moved,
                                },
                            });
                        } else {
                            events.push(Event::WindowEvent {
                                window_id,
                                event: AxisMotion {
                                    device_id,
                                    axis: i as u32,
                                    value: x,
                                },
                            });
                        }
                    }
                }
                for event in events {
                    callback(event);
                }
            }

            X11Event::XinputEnter(xev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                let window = xev.event as xproto::Window;
                let window_id = mkwid(window);
                let device_id = mkdid(xev.deviceid as xinput::DeviceId);

                if let Some(all_info) = DeviceInfo::get(&wt.xconn, super::ALL_DEVICES.into()) {
                    let mut devices = self.devices.borrow_mut();
                    for device_info in all_info.iter() {
                        if device_info.deviceid == xev.sourceid as _
                                // This is needed for resetting to work correctly on i3, and
                                // presumably some other WMs. On those, `XI_Enter` doesn't include
                                // the physical device ID, so both `sourceid` and `deviceid` are
                                // the virtual device.
                                || device_info.attachment == xev.sourceid as _
                        {
                            let device_id = DeviceId(device_info.deviceid as _);
                            if let Some(device) = devices.get_mut(&device_id) {
                                device.reset_scroll_position(device_info);
                            }
                        }
                    }
                }

                if self.window_exists(window) {
                    callback(Event::WindowEvent {
                        window_id,
                        event: CursorEntered { device_id },
                    });

                    let position = PhysicalPosition::new(
                        xinput_fp1616_to_float(xev.event_x),
                        xinput_fp1616_to_float(xev.event_y),
                    );

                    callback(Event::WindowEvent {
                        window_id,
                        event: CursorMoved {
                            device_id,
                            position,
                        },
                    });
                }
            }
            X11Event::XinputLeave(xev) => {
                let window = xev.event as xproto::Window;

                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                // Leave, FocusIn, and FocusOut can be received by a window that's already
                // been destroyed, which the user presumably doesn't want to deal with.
                let window_closed = !self.window_exists(window);
                if !window_closed {
                    callback(Event::WindowEvent {
                        window_id: mkwid(window),
                        event: CursorLeft {
                            device_id: mkdid(xev.deviceid as xinput::DeviceId),
                        },
                    });
                }
            }
            X11Event::XinputFocusIn(xev) => {
                let window = xev.event as xproto::Window;

                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                if let Some(ime) = wt.ime.as_ref() {
                    ime.borrow_mut()
                        .focus_window(window)
                        .expect("Failed to focus input context");
                }

                if self.active_window != Some(window) {
                    self.active_window = Some(window);

                    wt.update_listen_device_events(true);

                    let window_id = mkwid(window);
                    let position = PhysicalPosition::new(
                        xinput_fp1616_to_float(xev.event_x),
                        xinput_fp1616_to_float(xev.event_y),
                    );

                    if let Some(window) = self.with_window(window, Arc::clone) {
                        window.shared_state_lock().has_focus = true;
                    }

                    callback(Event::WindowEvent {
                        window_id,
                        event: Focused(true),
                    });

                    let modifiers: crate::keyboard::ModifiersState =
                        self.kb_state.mods_state().into();
                    if !modifiers.is_empty() {
                        callback(Event::WindowEvent {
                            window_id,
                            event: WindowEvent::ModifiersChanged(modifiers.into()),
                        });
                    }

                    // The deviceid for this event is for a keyboard instead of a pointer,
                    // so we have to do a little extra work.
                    let pointer_id = self
                        .devices
                        .borrow()
                        .get(&DeviceId(xev.deviceid as xinput::DeviceId))
                        .map(|device| device.attachment)
                        .unwrap_or(2);

                    callback(Event::WindowEvent {
                        window_id,
                        event: CursorMoved {
                            device_id: mkdid(pointer_id as _),
                            position,
                        },
                    });

                    // Issue key press events for all pressed keys
                    Self::handle_pressed_keys(
                        wt,
                        window_id,
                        ElementState::Pressed,
                        &mut self.kb_state,
                        &mut callback,
                    );
                }
            }
            X11Event::XinputFocusOut(xev) => {
                let window = xev.event as xproto::Window;

                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                if !self.window_exists(window) {
                    return;
                }

                if let Some(ime) = wt.ime.as_ref() {
                    ime.borrow_mut()
                        .unfocus_window(window)
                        .expect("Failed to focus input context");
                }

                if self.active_window.take() == Some(window) {
                    let window_id = mkwid(window);

                    wt.update_listen_device_events(false);

                    // Issue key release events for all pressed keys
                    Self::handle_pressed_keys(
                        wt,
                        window_id,
                        ElementState::Released,
                        &mut self.kb_state,
                        &mut callback,
                    );
                    // Clear this so detecting key repeats is consistently handled when the
                    // window regains focus.
                    self.held_key_press = None;

                    callback(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::ModifiersChanged(ModifiersState::empty().into()),
                    });

                    if let Some(window) = self.with_window(window, Arc::clone) {
                        window.shared_state_lock().has_focus = false;
                    }

                    callback(Event::WindowEvent {
                        window_id,
                        event: Focused(false),
                    })
                }
            }

            X11Event::XinputTouchBegin(xev)
            | X11Event::XinputTouchUpdate(xev)
            | X11Event::XinputTouchEnd(xev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                let window = xev.event as xproto::Window;
                let window_id = mkwid(window);
                let phase = match event {
                    X11Event::XinputTouchBegin(_) => TouchPhase::Started,
                    X11Event::XinputTouchUpdate(_) => TouchPhase::Moved,
                    X11Event::XinputTouchEnd(_) => TouchPhase::Ended,
                    _ => unreachable!(),
                };
                if self.window_exists(window) {
                    let id = xev.detail as u64;
                    let location = PhysicalPosition::new(
                        xinput_fp1616_to_float(xev.event_x),
                        xinput_fp1616_to_float(xev.event_y),
                    );

                    // Mouse cursor position changes when touch events are received.
                    // Only the first concurrently active touch ID moves the mouse cursor.
                    if is_first_touch(&mut self.first_touch, &mut self.num_touch, id, phase) {
                        callback(Event::WindowEvent {
                            window_id,
                            event: WindowEvent::CursorMoved {
                                device_id: mkdid(util::VIRTUAL_CORE_POINTER),
                                position: location.cast(),
                            },
                        });
                    }

                    callback(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::Touch(Touch {
                            device_id: mkdid(xev.deviceid as xinput::DeviceId),
                            phase,
                            location,
                            force: None, // TODO
                            id,
                        }),
                    })
                }
            }

            X11Event::XinputRawButtonPress(xev) | X11Event::XinputRawButtonRelease(xev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                if xev
                    .flags
                    .contains(xinput::PointerEventFlags::POINTER_EMULATED)
                {
                    callback(Event::DeviceEvent {
                        device_id: mkdid(xev.deviceid as xinput::DeviceId),
                        event: DeviceEvent::Button {
                            button: xev.detail,
                            state: match event {
                                X11Event::XinputRawButtonPress(_) => Pressed,
                                X11Event::XinputRawButtonRelease(_) => Released,
                                _ => unreachable!(),
                            },
                        },
                    });
                }
            }

            X11Event::XinputRawMotion(xev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                let did = mkdid(xev.deviceid as xinput::DeviceId);

                let mut mouse_delta = (0.0, 0.0);
                let mut scroll_delta = (0.0, 0.0);

                let mask = bytemuck::cast_slice::<u32, u8>(&xev.valuator_mask);
                let mut values = &*xev.axisvalues;

                for i in 0..mask.len() * 8 {
                    let byte_index = i / 8;
                    let bit_index = i % 8;

                    if mask[byte_index] & (1 << bit_index) == 0 {
                        continue;
                    }

                    // This mask is set, get the value.
                    let x = {
                        let (value, rest) = values.split_first().unwrap();
                        values = rest;
                        xinput_fp3232_to_float(*value)
                    };

                    // We assume that every XInput2 device with analog axes is a pointing device emitting
                    // relative coordinates.
                    match i {
                        0 => mouse_delta.0 = x,
                        1 => mouse_delta.1 = x,
                        2 => scroll_delta.0 = x as f32,
                        3 => scroll_delta.1 = x as f32,
                        _ => {}
                    }
                    callback(Event::DeviceEvent {
                        device_id: did,
                        event: DeviceEvent::Motion {
                            axis: i as u32,
                            value: x,
                        },
                    });
                }
                if mouse_delta != (0.0, 0.0) {
                    callback(Event::DeviceEvent {
                        device_id: did,
                        event: DeviceEvent::MouseMotion { delta: mouse_delta },
                    });
                }
                if scroll_delta != (0.0, 0.0) {
                    callback(Event::DeviceEvent {
                        device_id: did,
                        event: DeviceEvent::MouseWheel {
                            delta: LineDelta(scroll_delta.0, scroll_delta.1),
                        },
                    });
                }
            }
            X11Event::XinputRawKeyPress(xev) | X11Event::XinputRawKeyRelease(xev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                let state = match event {
                    X11Event::XinputRawKeyPress(_) => Pressed,
                    X11Event::XinputRawKeyRelease(_) => Released,
                    _ => unreachable!(),
                };

                let device_id = mkdid(xev.sourceid as xinput::DeviceId);
                let keycode = xev.detail;
                if keycode < KEYCODE_OFFSET as u32 {
                    return;
                }
                let physical_key = keymap::raw_keycode_to_keycode(keycode);

                callback(Event::DeviceEvent {
                    device_id,
                    event: DeviceEvent::Key(RawKeyEvent {
                        physical_key,
                        state,
                    }),
                });
            }

            X11Event::XinputHierarchy(xev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                for info in &xev.infos {
                    if info.flags.contains(
                        xinput::HierarchyMask::MASTER_ADDED | xinput::HierarchyMask::MASTER_REMOVED,
                    ) {
                        self.init_device(info.deviceid as xinput::DeviceId);
                        callback(Event::DeviceEvent {
                            device_id: mkdid(info.deviceid as xinput::DeviceId),
                            event: DeviceEvent::Added,
                        });
                    } else if info.flags.contains(
                        xinput::HierarchyMask::SLAVE_ADDED | xinput::HierarchyMask::SLAVE_REMOVED,
                    ) {
                        callback(Event::DeviceEvent {
                            device_id: mkdid(info.deviceid as xinput::DeviceId),
                            event: DeviceEvent::Removed,
                        });
                        let mut devices = self.devices.borrow_mut();
                        devices.remove(&DeviceId(info.deviceid as xinput::DeviceId));
                    }
                }
            }

            X11Event::XkbNewKeyboardNotify(xev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                let keycodes_changed = xev.changed.contains(xkb::NKNDetail::KEYCODES);
                let geometry_changed = xev.changed.contains(xkb::NKNDetail::GEOMETRY);

                if xev.device_id as i32 == self.kb_state.core_keyboard_id
                    && (keycodes_changed || geometry_changed)
                {
                    unsafe { self.kb_state.init_with_x11_keymap() };
                }
            }
            X11Event::XkbStateNotify(xev) => {
                // Set the timestamp.
                wt.xconn.set_timestamp(xev.time as xproto::Timestamp);

                let prev_mods = self.kb_state.mods_state();
                self.kb_state.update_modifiers(
                    xev.base_mods.into(),
                    xev.latched_mods.into(),
                    xev.locked_mods.into(),
                    xev.base_group as u32,
                    xev.latched_group as u32,
                    xev.locked_group.into(),
                );
                let new_mods = self.kb_state.mods_state();
                if prev_mods != new_mods {
                    if let Some(window) = self.active_window {
                        callback(Event::WindowEvent {
                            window_id: mkwid(window),
                            event: WindowEvent::ModifiersChanged(
                                Into::<ModifiersState>::into(new_mods).into(),
                            ),
                        });
                    }
                }
            }

            X11Event::RandrNotify(_) => {
                // In the future, it would be quite easy to emit monitor hotplug events.
                let prev_list = wt.xconn.invalidate_cached_monitor_list();
                if let Some(prev_list) = prev_list {
                    let new_list = wt
                        .xconn
                        .available_monitors()
                        .expect("Failed to get monitor list");
                    for new_monitor in new_list {
                        // Previous list may be empty, in case of disconnecting and
                        // reconnecting the only one monitor. We still need to emit events in
                        // this case.
                        let maybe_prev_scale_factor = prev_list
                            .iter()
                            .find(|prev_monitor| prev_monitor.name == new_monitor.name)
                            .map(|prev_monitor| prev_monitor.scale_factor);
                        if Some(new_monitor.scale_factor) != maybe_prev_scale_factor {
                            for (window_id, window) in wt.windows.borrow().iter() {
                                if let Some(window) = window.upgrade() {
                                    // Check if the window is on this monitor
                                    let monitor = window.shared_state_lock().last_monitor.clone();
                                    if monitor.name == new_monitor.name {
                                        let (width, height) = window.inner_size_physical();
                                        let (new_width, new_height) = window.adjust_for_dpi(
                                            // If we couldn't determine the previous scale
                                            // factor (e.g., because all monitors were closed
                                            // before), just pick whatever the current monitor
                                            // has set as a baseline.
                                            maybe_prev_scale_factor.unwrap_or(monitor.scale_factor),
                                            new_monitor.scale_factor,
                                            width,
                                            height,
                                            &window.shared_state_lock(),
                                        );

                                        let window_id = crate::window::WindowId(*window_id);
                                        let old_inner_size = PhysicalSize::new(width, height);
                                        let inner_size = Arc::new(Mutex::new(PhysicalSize::new(
                                            new_width, new_height,
                                        )));
                                        callback(Event::WindowEvent {
                                            window_id,
                                            event: WindowEvent::ScaleFactorChanged {
                                                scale_factor: new_monitor.scale_factor,
                                                inner_size_writer: InnerSizeWriter::new(
                                                    Arc::downgrade(&inner_size),
                                                ),
                                            },
                                        });

                                        let new_inner_size = *inner_size.lock().unwrap();
                                        drop(inner_size);

                                        if new_inner_size != old_inner_size {
                                            let (new_width, new_height) = new_inner_size.into();
                                            window
                                                .request_inner_size_physical(new_width, new_height);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            _ => {
                // Don't care, lol
            }
        }

        if let Some(ime) = wt.ime.as_ref() {
            let mut ime = ime.borrow_mut();

            // Handle IME requests.
            if let Ok(request) = self.ime_requests.try_recv() {
                match request {
                    ime::ImeRequest::Position(window_id, x, y) => {
                        ime.set_spot(window_id, x, y)
                            .expect("Failed to set IME spot");
                    }
                    ime::ImeRequest::Allow(window_id, allowed) => {
                        ime.set_ime_allowed(window_id, allowed)
                            .expect("Failed to set IME allowed");
                    }
                }
            }

            let (window, event) = match ime.next_ime_event() {
                Some((window, event)) => (window, event),
                None => return,
            };

            match event {
                ime::ImeEvent::Enabled => {
                    callback(Event::WindowEvent {
                        window_id: mkwid(window),
                        event: WindowEvent::Ime(Ime::Enabled),
                    });
                }
                ime::ImeEvent::Start => {
                    self.is_composing = true;
                    callback(Event::WindowEvent {
                        window_id: mkwid(window),
                        event: WindowEvent::Ime(Ime::Preedit("".to_owned(), None)),
                    });
                }
                ime::ImeEvent::Update(text, position) => {
                    if self.is_composing {
                        callback(Event::WindowEvent {
                            window_id: mkwid(window),
                            event: WindowEvent::Ime(Ime::Preedit(
                                text,
                                position.map(|position| (position, position)),
                            )),
                        });
                    }
                }
                ime::ImeEvent::Commit(text) => {
                    self.is_composing = false;
                    callback(Event::WindowEvent {
                        window_id: mkwid(window),
                        event: WindowEvent::Ime(Ime::Preedit(String::new(), None)),
                    });
                    callback(Event::WindowEvent {
                        window_id: mkwid(window),
                        event: WindowEvent::Ime(Ime::Commit(text)),
                    });
                }
                ime::ImeEvent::End => {
                    self.is_composing = false;
                    // Issue empty preedit on `Done`.
                    callback(Event::WindowEvent {
                        window_id: mkwid(window),
                        event: WindowEvent::Ime(Ime::Preedit(String::new(), None)),
                    });
                }
                ime::ImeEvent::Disabled => {
                    self.is_composing = false;
                    callback(Event::WindowEvent {
                        window_id: mkwid(window),
                        event: WindowEvent::Ime(Ime::Disabled),
                    });
                }
            }
        }
    }

    fn handle_pressed_keys<F>(
        wt: &super::EventLoopWindowTarget<T>,
        window_id: crate::window::WindowId,
        state: ElementState,
        kb_state: &mut KbdState,
        callback: &mut F,
    ) where
        F: FnMut(Event<T>),
    {
        let device_id = mkdid(util::VIRTUAL_CORE_KEYBOARD);

        // Update modifiers state and emit key events based on which keys are currently pressed.
        for keycode in wt
            .xconn
            .query_keymap()
            .into_iter()
            .filter(|k| *k >= KEYCODE_OFFSET)
        {
            let keycode = keycode as u32;
            let event = kb_state.process_key_event(keycode, state, false);
            callback(Event::WindowEvent {
                window_id,
                event: WindowEvent::KeyboardInput {
                    device_id,
                    event,
                    is_synthetic: true,
                },
            });
        }
    }
}

fn is_first_touch(first: &mut Option<u64>, num: &mut u32, id: u64, phase: TouchPhase) -> bool {
    match phase {
        TouchPhase::Started => {
            if *num == 0 {
                *first = Some(id);
            }
            *num += 1;
        }
        TouchPhase::Cancelled | TouchPhase::Ended => {
            if *first == Some(id) {
                *first = None;
            }
            *num = num.saturating_sub(1);
        }
        _ => (),
    }

    *first == Some(id)
}
