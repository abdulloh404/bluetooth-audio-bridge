//! จัดการอายุของ native PipeWire engine โดยไม่ส่ง PCM ผ่าน Rust

use std::{ffi::{c_char, c_int, CStr}, marker::PhantomData, ptr::NonNull, rc::Rc};

#[derive(Clone, Copy, Debug)]
pub struct Levels {
    pub phone_gain: f32,
    pub desktop_gain: f32,
    pub master_gain: f32,
    pub phone_mute: bool,
    pub desktop_mute: bool,
    pub master_mute: bool,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub pipewire_connected: bool,
    pub routing_enabled: bool,
    pub policy_ready: bool,
    pub inputs_detected: u32,
    pub inputs_routed: u32,
    pub default_output_name: String,
    pub last_error: String,
    pub routes: Vec<RouteStatus>,
}

#[derive(Clone, Debug)]
pub struct RouteStatus {
    pub input_name: String,
    pub input_address: String,
    pub output_name: String,
    pub ready: bool,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub last_error: String,
}

#[repr(C)]
struct NativeLevels {
    phone_gain: f32,
    desktop_gain: f32,
    master_gain: f32,
    phone_mute: u8,
    desktop_mute: u8,
    master_mute: u8,
}

#[repr(C)]
struct NativeStatus {
    pipewire_connected: u8,
    routing_enabled: u8,
    policy_ready: u8,
    inputs_detected: u32,
    inputs_routed: u32,
    default_output_name: [c_char; 512],
    last_error: [c_char; 512],
}

#[repr(C)]
struct NativeRouteStatus {
    input_name: [c_char; 512],
    input_address: [c_char; 64],
    output_name: [c_char; 512],
    ready: u8,
    codec: [c_char; 64],
    sample_rate: u32,
    channels: u32,
    last_error: [c_char; 512],
}

#[repr(C)]
struct NativeEngine { _private: [u8; 0] }

extern "C" {
    fn bab_engine_create(error: *mut c_char, size: usize) -> *mut NativeEngine;
    fn bab_engine_set_levels(engine: *mut NativeEngine, levels: *const NativeLevels, error: *mut c_char, size: usize) -> c_int;
    fn bab_engine_set_enabled(engine: *mut NativeEngine, enabled: u8, error: *mut c_char, size: usize) -> c_int;
    fn bab_engine_tick(engine: *mut NativeEngine, error: *mut c_char, size: usize) -> c_int;
    fn bab_engine_status(engine: *const NativeEngine, status: *mut NativeStatus);
    fn bab_engine_route_count(engine: *const NativeEngine) -> u32;
    fn bab_engine_route_status(engine: *const NativeEngine, index: u32, status: *mut NativeRouteStatus) -> c_int;
    fn bab_engine_destroy(engine: *mut NativeEngine);
}

/// เป็นเจ้าของ engine เพียงรายเดียว และจำกัดการควบคุมไว้บน Rust thread เดิม
pub struct Engine {
    handle: NonNull<NativeEngine>,
    _thread: PhantomData<Rc<()>>,
}

fn native_text(value: &[c_char]) -> String {
    // Native API เติม NUL ภายใน array ทุกครั้งก่อนคืนค่า
    unsafe { CStr::from_ptr(value.as_ptr()) }.to_string_lossy().into_owned()
}

fn result(code: c_int, error: &[c_char]) -> Result<(), String> {
    if code == 0 { Ok(()) } else { Err(native_text(error)) }
}

impl Engine {
    pub fn new() -> Result<Self, String> {
        let mut error = [0; 1024];
        let handle = unsafe { bab_engine_create(error.as_mut_ptr(), error.len()) };
        let handle = NonNull::new(handle).ok_or_else(|| native_text(&error))?;
        Ok(Self { handle, _thread: PhantomData })
    }

    pub fn set_levels(&mut self, levels: Levels) -> Result<(), String> {
        let levels = NativeLevels {
            phone_gain: levels.phone_gain,
            desktop_gain: levels.desktop_gain,
            master_gain: levels.master_gain,
            phone_mute: levels.phone_mute.into(),
            desktop_mute: levels.desktop_mute.into(),
            master_mute: levels.master_mute.into(),
        };
        let mut error = [0; 1024];
        let code = unsafe { bab_engine_set_levels(self.handle.as_ptr(), &levels, error.as_mut_ptr(), error.len()) };
        result(code, &error)
    }

    pub fn set_enabled(&mut self, enabled: bool) -> Result<(), String> {
        let mut error = [0; 1024];
        let code = unsafe { bab_engine_set_enabled(self.handle.as_ptr(), enabled.into(), error.as_mut_ptr(), error.len()) };
        result(code, &error)
    }

    pub fn tick(&mut self) -> Result<(), String> {
        let mut error = [0; 1024];
        let code = unsafe { bab_engine_tick(self.handle.as_ptr(), error.as_mut_ptr(), error.len()) };
        result(code, &error)
    }

    pub fn status(&self) -> Status {
        // NativeStatus มีเพียงตัวเลขและ array จึงกำหนดค่าเริ่มต้นเป็นศูนย์ได้
        let mut native: NativeStatus = unsafe { std::mem::zeroed() };
        unsafe { bab_engine_status(self.handle.as_ptr(), &mut native) };
        let mut routes = Vec::new();
        let count = unsafe { bab_engine_route_count(self.handle.as_ptr()) };
        for index in 0..count {
            let mut route: NativeRouteStatus = unsafe { std::mem::zeroed() };
            if unsafe { bab_engine_route_status(self.handle.as_ptr(), index, &mut route) } != 0 { continue; }
            routes.push(RouteStatus {
                input_name: native_text(&route.input_name),
                input_address: native_text(&route.input_address),
                output_name: native_text(&route.output_name),
                ready: route.ready != 0,
                codec: native_text(&route.codec),
                sample_rate: route.sample_rate,
                channels: route.channels,
                last_error: native_text(&route.last_error),
            });
        }
        Status {
            pipewire_connected: native.pipewire_connected != 0,
            routing_enabled: native.routing_enabled != 0,
            policy_ready: native.policy_ready != 0,
            inputs_detected: native.inputs_detected,
            inputs_routed: native.inputs_routed,
            default_output_name: native_text(&native.default_output_name),
            last_error: native_text(&native.last_error),
            routes,
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // NonNull นี้สร้างจาก C++ และคืน ownership เพียงครั้งเดียว
        unsafe { bab_engine_destroy(self.handle.as_ptr()) };
    }
}
