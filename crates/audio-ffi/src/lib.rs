//! จัดการอายุของ native PipeWire engine โดยไม่ส่ง PCM ผ่าน Rust

use std::{ffi::{c_char, c_int, CStr, CString}, marker::PhantomData, ptr::NonNull, rc::Rc};

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub virtual_sink_name: String,
    pub iphone_address: String,
    pub headphones_address: String,
    pub allow_codec_fallback: bool,
}

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
    pub virtual_sink_ready: bool,
    pub phone_ready: bool,
    pub headphones_ready: bool,
    pub routing_enabled: bool,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub phone_stream_state: String,
    pub output_stream_state: String,
    pub last_error: String,
}

#[repr(C)]
struct NativeConfig {
    virtual_sink_name: *const c_char,
    iphone_address: *const c_char,
    headphones_address: *const c_char,
    allow_codec_fallback: u8,
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
    virtual_sink_ready: u8,
    phone_ready: u8,
    headphones_ready: u8,
    routing_enabled: u8,
    sample_rate: u32,
    channels: u32,
    codec: [c_char; 64],
    phone_stream_state: [c_char; 64],
    output_stream_state: [c_char; 64],
    last_error: [c_char; 512],
}

#[repr(C)]
struct NativeEngine { _private: [u8; 0] }

extern "C" {
    fn bab_engine_create(config: *const NativeConfig, error: *mut c_char, size: usize) -> *mut NativeEngine;
    fn bab_engine_set_levels(engine: *mut NativeEngine, levels: *const NativeLevels, error: *mut c_char, size: usize) -> c_int;
    fn bab_engine_set_enabled(engine: *mut NativeEngine, enabled: u8, error: *mut c_char, size: usize) -> c_int;
    fn bab_engine_tick(engine: *mut NativeEngine, error: *mut c_char, size: usize) -> c_int;
    fn bab_engine_status(engine: *const NativeEngine, status: *mut NativeStatus);
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
    pub fn new(config: &EngineConfig) -> Result<Self, String> {
        let sink = CString::new(config.virtual_sink_name.as_str()).map_err(|e| e.to_string())?;
        let phone = CString::new(config.iphone_address.as_str()).map_err(|e| e.to_string())?;
        let headphones = CString::new(config.headphones_address.as_str()).map_err(|e| e.to_string())?;
        let config = NativeConfig {
            virtual_sink_name: sink.as_ptr(),
            iphone_address: phone.as_ptr(),
            headphones_address: headphones.as_ptr(),
            allow_codec_fallback: config.allow_codec_fallback.into(),
        };
        let mut error = [0; 1024];
        // C++ คัดลอก config ก่อนคืนค่า จึงไม่เก็บ pointer ของ CString เหล่านี้
        let handle = unsafe { bab_engine_create(&config, error.as_mut_ptr(), error.len()) };
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
        Status {
            pipewire_connected: native.pipewire_connected != 0,
            virtual_sink_ready: native.virtual_sink_ready != 0,
            phone_ready: native.phone_ready != 0,
            headphones_ready: native.headphones_ready != 0,
            routing_enabled: native.routing_enabled != 0,
            codec: native_text(&native.codec),
            sample_rate: native.sample_rate,
            channels: native.channels,
            phone_stream_state: native_text(&native.phone_stream_state),
            output_stream_state: native_text(&native.output_stream_state),
            last_error: native_text(&native.last_error),
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // NonNull นี้สร้างจาก C++ และคืน ownership เพียงครั้งเดียว
        unsafe { bab_engine_destroy(self.handle.as_ptr()) };
    }
}
