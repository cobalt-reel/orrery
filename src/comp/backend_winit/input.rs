//! The host windows' input as Smithay `InputEvent`s (ported from Smithay's winit backend, whose event types cannot
//! be built outside it): keys, pointer motion (window-local, relative to the window's size), buttons, wheels. Touch
//! is dropped.

use std::path::PathBuf;

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisRelativeDirection, AxisSource, ButtonState, Device, DeviceCapability, Event,
    InputBackend, KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
    PointerMotionAbsoluteEvent, UnusedEvent,
};
use smithay::reexports::winit::event::{ElementState, MouseButton, MouseScrollDelta};

/// The `InputBackend` of the host windows.
#[derive(Debug)]
pub struct Nested;

/// All host input comes from one virtual device.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct HostDevice;

impl Device for HostDevice {
    fn id(&self) -> String {
        "winit".into()
    }

    fn name(&self) -> String {
        "nested host input".into()
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(capability, DeviceCapability::Keyboard | DeviceCapability::Pointer)
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}

/// Every event's time, in microseconds.
macro_rules! timed {
    ($t:ty) => {
        impl Event<Nested> for $t {
            fn time(&self) -> u64 {
                self.time
            }

            fn device(&self) -> HostDevice {
                HostDevice
            }
        }
    };
}

#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub time: u64,
    /// The evdev scancode.
    pub scancode: u32,
    /// Keys down, this one included.
    pub count: u32,
    pub state: ElementState,
}
timed!(Key);

impl KeyboardKeyEvent<Nested> for Key {
    fn key_code(&self) -> Keycode {
        // xkb keycodes are evdev's plus 8.
        (self.scancode + 8).into()
    }

    fn state(&self) -> KeyState {
        match self.state {
            ElementState::Pressed => KeyState::Pressed,
            ElementState::Released => KeyState::Released,
        }
    }

    fn count(&self) -> u32 {
        self.count
    }
}

/// The pointer moved inside a window: `x`, `y` from 0 to 1 across it, `local` in its pixels.
#[derive(Debug, Clone, Copy)]
pub struct Motion {
    pub time: u64,
    pub x: f64,
    pub y: f64,
    pub local: (f64, f64),
}
timed!(Motion);

impl PointerMotionAbsoluteEvent<Nested> for Motion {}

impl AbsolutePositionEvent<Nested> for Motion {
    fn x(&self) -> f64 {
        self.local.0
    }

    fn y(&self) -> f64 {
        self.local.1
    }

    fn x_transformed(&self, width: i32) -> f64 {
        f64::max(self.x * f64::from(width), 0.0)
    }

    fn y_transformed(&self, height: i32) -> f64 {
        f64::max(self.y * f64::from(height), 0.0)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Button {
    pub time: u64,
    /// The evdev button code (`button_code`).
    pub code: u32,
    pub state: ElementState,
}
timed!(Button);

impl PointerButtonEvent<Nested> for Button {
    fn button_code(&self) -> u32 {
        self.code
    }

    fn state(&self) -> ButtonState {
        match self.state {
            ElementState::Pressed => ButtonState::Pressed,
            ElementState::Released => ButtonState::Released,
        }
    }
}

/// A winit button as evdev's code; X11 numbers its extra buttons its own way.
pub fn button_code(button: MouseButton, x11: bool) -> u32 {
    match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Forward => 0x115,
        MouseButton::Back => 0x116,
        MouseButton::Other(b) if x11 => match u32::from(b) {
            0 => 0,
            1 => 0x110,
            2 => 0x112,
            3 => 0x111,
            b => (b + 0x113).saturating_sub(8),
        },
        MouseButton::Other(b) => u32::from(b),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Wheel {
    pub time: u64,
    pub delta: MouseScrollDelta,
}
timed!(Wheel);

impl PointerAxisEvent<Nested> for Wheel {
    fn source(&self) -> AxisSource {
        match self.delta {
            MouseScrollDelta::LineDelta(..) => AxisSource::Wheel,
            MouseScrollDelta::PixelDelta(_) => AxisSource::Continuous,
        }
    }

    fn amount(&self, axis: Axis) -> Option<f64> {
        match (axis, self.delta) {
            (Axis::Horizontal, MouseScrollDelta::PixelDelta(d)) => Some(-d.x),
            (Axis::Vertical, MouseScrollDelta::PixelDelta(d)) => Some(-d.y),
            (_, MouseScrollDelta::LineDelta(..)) => None,
        }
    }

    fn amount_v120(&self, axis: Axis) -> Option<f64> {
        match (axis, self.delta) {
            (Axis::Horizontal, MouseScrollDelta::LineDelta(x, _)) => Some(-f64::from(x) * 120.0),
            (Axis::Vertical, MouseScrollDelta::LineDelta(_, y)) => Some(-f64::from(y) * 120.0),
            (_, MouseScrollDelta::PixelDelta(_)) => None,
        }
    }

    fn relative_direction(&self, _axis: Axis) -> AxisRelativeDirection {
        AxisRelativeDirection::Identical
    }
}

impl InputBackend for Nested {
    type Device = HostDevice;
    type KeyboardKeyEvent = Key;
    type PointerAxisEvent = Wheel;
    type PointerButtonEvent = Button;
    type PointerMotionEvent = UnusedEvent;
    type PointerMotionAbsoluteEvent = Motion;

    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;

    type TouchDownEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;

    type SwitchToggleEvent = UnusedEvent;

    type SpecialEvent = UnusedEvent;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_map_to_evdev_codes() {
        assert_eq!(button_code(MouseButton::Left, false), 0x110);
        assert_eq!(button_code(MouseButton::Right, true), 0x111);
        assert_eq!(
            button_code(MouseButton::Other(0x113), false),
            0x113,
            "Wayland passes evdev codes through"
        );
        assert_eq!(
            button_code(MouseButton::Other(8), true),
            0x113,
            "X11's first extra button"
        );
        assert_eq!(button_code(MouseButton::Other(2), true), 0x112, "X11's middle");
    }
}
