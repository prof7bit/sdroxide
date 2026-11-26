//! The RADE V2 text channel: a station's callsign, streamed one bit per modem
//! frame for as long as it transmits.
//!
//! Unlike V1, which names a station once in its End-of-Over frame
//! ([`crate::text`]), V2 repeats an LDPC-coded message throughout the over, so
//! a receiver learns the callsign while the station is still talking and then
//! hears it again every few seconds.
//!
//! This is a safe wrapper around FreeDV's own `rade_text.cpp`, vendored under
//! `cpp/` and compiled by `build.rs`.

use std::ffi::{c_char, c_int, c_void};

type Handle = *mut c_void;
type OnText = unsafe extern "C" fn(Handle, *const c_char, c_int, *mut c_void);

// `cpp/pipeline/rade_text.h`.
unsafe extern "C" {
    fn rade_text_create() -> Handle;
    fn rade_text_destroy(ptr: Handle);
    fn rade_text_generate_tx_string(ptr: Handle, str: *const c_char, strlength: c_int);
    fn rade_text_tx_next_symbol(ptr: Handle) -> f32;
    fn rade_text_set_rx_callback(ptr: Handle, f: OnText, state: *mut c_void);
    fn rade_text_rx_symbol(ptr: Handle, sym: f32);
    fn rade_text_enable_stats_output(ptr: Handle, enable: c_int);
    fn rade_text_rx_reset(ptr: Handle);
}

/// Called by the C++ side from inside `rade_text_rx_symbol`.
unsafe extern "C" fn on_text(_: Handle, txt: *const c_char, len: c_int, state: *mut c_void) {
    if txt.is_null() || state.is_null() || len <= 0 {
        return;
    }
    // Safety: `txt` holds `len` bytes for the duration of this call, and
    // `state` is the `Vec` owned by the `TextV2` that is calling us.
    unsafe {
        let bytes = std::slice::from_raw_parts(txt.cast::<u8>(), len as usize);
        let heard = &mut *state.cast::<Vec<String>>();
        heard.push(String::from_utf8_lossy(bytes).into_owned());
    }
}

/// One transmit stream and one receive decoder.
pub struct TextV2 {
    handle: Handle,
    /// Decoded messages not yet taken. A raw pointer because the C++ side
    /// keeps it: it must stay put while this struct moves.
    heard: *mut Vec<String>,
}

// Safety: the C++ object is only reached through `&mut self`, and the one piece
// of file-scope state it has is used by the statistics output that `new` turns
// off.
unsafe impl Send for TextV2 {}

impl TextV2 {
    pub fn new() -> Self {
        let heard = Box::into_raw(Box::new(Vec::new()));
        // Safety: `rade_text_create` returns a fresh object (it asserts rather
        // than return null), and `heard` outlives it.
        let handle = unsafe {
            let h = rade_text_create();
            rade_text_enable_stats_output(h, 0);
            rade_text_set_rx_callback(h, on_text, heard.cast());
            h
        };
        let mut text = TextV2 { handle, heard };
        text.set_message("");
        text
    }

    /// Set the callsign to transmit and start its cycle from the beginning.
    /// Characters outside A-Z, 0-9 and `/` are dropped by the encoder.
    pub fn set_message(&mut self, message: &str) {
        // The length is passed alongside, so no terminator is needed.
        let len = message.len().min(c_int::MAX as usize) as c_int;
        // Safety: the encoder reads at most `len` bytes and keeps no pointer.
        unsafe { rade_text_generate_tx_string(self.handle, message.as_ptr().cast(), len) };
    }

    /// The data symbol (+1.0 / -1.0) for the next modem frame.
    pub fn next_symbol(&mut self) -> f32 {
        // Safety: `self.handle` is valid for the lifetime of `self`.
        unsafe { rade_text_tx_next_symbol(self.handle) }
    }

    /// Feed one received soft-decision symbol, appending any message it
    /// completes to `out`.
    pub fn receive(&mut self, symbol: f32, out: &mut Vec<String>) {
        // The decoder divides by the symbol's magnitude, and a NaN would stay
        // in its combining sums.
        if !symbol.is_finite() || symbol == 0.0 {
            return;
        }
        // Safety: the callback only touches `self.heard`, which nothing else
        // borrows during the call.
        unsafe {
            rade_text_rx_symbol(self.handle, symbol);
            out.append(&mut *self.heard);
        }
    }

    /// Forget everything received so far. For when the modem loses sync: the
    /// position in the far end's cycle is unknown afterwards.
    pub fn reset_rx(&mut self) {
        // Safety: as above.
        unsafe {
            rade_text_rx_reset(self.handle);
            (*self.heard).clear();
        }
    }
}

impl Default for TextV2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TextV2 {
    fn drop(&mut self) {
        // Safety: both pointers came from `new` and are released exactly once,
        // the C++ object first since it holds `heard`.
        unsafe {
            rade_text_destroy(self.handle);
            drop(Box::from_raw(self.heard));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stream `from`'s message into `to` and collect what comes out.
    fn stream(from: &mut TextV2, to: &mut TextV2, symbols: usize) -> Vec<String> {
        let mut heard = Vec::new();
        for _ in 0..symbols {
            let s = from.next_symbol();
            to.receive(s, &mut heard);
        }
        heard
    }

    #[test]
    fn a_callsign_comes_through_and_repeats() {
        let (mut tx, mut rx) = (TextV2::new(), TextV2::new());
        tx.set_message("aa1zzz");
        let heard = stream(&mut tx, &mut rx, 112 * 4);
        assert!(heard.len() >= 2, "{heard:?}");
        assert!(heard.iter().all(|c| c == "AA1ZZZ"), "{heard:?}");
    }

    #[test]
    fn a_long_callsign_is_reassembled_after_joining_late() {
        let (mut tx, mut rx) = (TextV2::new(), TextV2::new());
        tx.set_message("VE3/AA1ZZZ/MM");
        for _ in 0..77 {
            tx.next_symbol();
        }
        let heard = stream(&mut tx, &mut rx, 112 * 8);
        assert!(heard.iter().any(|c| c == "VE3/AA1ZZZ/MM"), "{heard:?}");
    }
}
