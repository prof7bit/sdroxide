//! A narrow bandpass for the transmit audio.
//!
//! RADE V1 limits its peaks inside the modulator and, as `rade_c` is built
//! here, filters nothing afterwards, which leaves a wide floor of splatter on
//! both sides of the signal. V2 has a filter of its own, but a wide one,
//! 300 to 2700 Hz. A sideband transmitter passes much of both on to the
//! neighbours, so this narrows each to little more than its carriers.
//!
//! The filter is linear phase, which the OFDM carriers need, and works on the
//! real audio at the modem rate. It builds the analytic signal of what it
//! passes along the way, so that V1's peaks, which filtering raises again, can
//! be limited by envelope to what the modulator itself keeps to.
//!
//! Limiting splatters in turn, so V1 alternates: limit, filter, and that
//! [`V1_LIMIT_PASSES`] times. Done once and left unfiltered, the neighbours
//! would only be some 12 dB better off than with no filter at all. Each filter
//! lets the peaks grow back a little, less with every pass; after the last they
//! stay within about 1 dB of the modulator's bound.

use num_complex::Complex32;

use crate::{MODEM_RATE, RadeVersion, TX_REAL_SCALE};

/// Odd, so that the delay is a whole number of samples. At 8 kHz this gives
/// about 60 dB of stop band with a transition [`TRANSITION_HZ`] wide.
const TAPS: usize = 195;
const TRANSITION_HZ: f64 = 150.0;
/// The Kaiser window's shape for 60 dB.
const KAISER_BETA: f64 = 0.1102 * (60.0 - 8.7);
/// How often V1 is limited and filtered again.
const V1_LIMIT_PASSES: usize = 3;

/// The flat part of each version's passband, 100 Hz or a little more beyond
/// the outermost carriers: V1's are at 750 to 2200 Hz, V2's at 1062.5 to 1875.
fn flat_hz(version: RadeVersion) -> (f64, f64) {
    match version {
        RadeVersion::V1 => (650.0, 2300.0),
        RadeVersion::V2 => (950.0, 2000.0),
    }
}

/// One run through the bandpass, a sample at a time.
struct Fir {
    /// The last [`TAPS`] input samples, written twice over so that they can
    /// always be read as one slice.
    history: Vec<f32>,
    at: usize,
}

impl Fir {
    fn new() -> Fir {
        Fir { history: vec![0.0; 2 * TAPS], at: 0 }
    }

    fn reset(&mut self) {
        self.history.fill(0.0);
        self.at = 0;
    }

    /// The analytic signal of what passes, half the filter's length behind.
    fn push(&mut self, x: f32, taps: &[Complex32]) -> Complex32 {
        self.history[self.at] = x;
        self.history[self.at + TAPS] = x;
        self.at = (self.at + 1) % TAPS;
        // Oldest first. The taps are used backwards, as a convolution does.
        self.history[self.at..self.at + TAPS]
            .iter()
            .zip(taps.iter().rev())
            .fold(Complex32::default(), |acc, (&x, &h)| acc + h * x)
    }
}

pub(crate) struct TxFilter {
    /// Complex taps: the real part is the bandpass, the whole its analytic
    /// signal.
    taps: Vec<Complex32>,
    /// The filter itself, and on V1 one more after each limiting pass.
    first: Fir,
    again: Vec<Fir>,
    /// The envelope each pass limits to.
    limit: f32,
}

impl TxFilter {
    pub(crate) fn new(version: RadeVersion) -> TxFilter {
        let (lo, hi) = flat_hz(version);
        // The -6 dB points lie half a transition outside the flat part.
        let (lo, hi) = (lo - TRANSITION_HZ / 2.0, hi + TRANSITION_HZ / 2.0);
        let half_width = (hi - lo) / 2.0 / MODEM_RATE;
        let centre = (hi + lo) / 2.0 / MODEM_RATE;
        let mid = (TAPS / 2) as f64;

        // A windowed-sinc lowpass of half the width ...
        let lowpass: Vec<f64> = (0..TAPS)
            .map(|i| {
                let n = i as f64 - mid;
                let x = std::f64::consts::TAU * half_width * n;
                let sinc = if n == 0.0 { 1.0 } else { x.sin() / x };
                let r = n / mid;
                sinc * bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt())
            })
            .collect();
        // ... with unit gain, moved up to the middle of the band. Twice the
        // gain, because only the positive frequencies are kept.
        let sum: f64 = lowpass.iter().sum();
        let taps = lowpass
            .iter()
            .enumerate()
            .map(|(i, &h)| {
                let phase = std::f64::consts::TAU * centre * (i as f64 - mid);
                let g = 2.0 * h / sum;
                Complex32::new((g * phase.cos()) as f32, (g * phase.sin()) as f32)
            })
            .collect();

        let passes = if version == RadeVersion::V1 { V1_LIMIT_PASSES } else { 0 };
        TxFilter {
            taps,
            first: Fir::new(),
            again: (0..passes).map(|_| Fir::new()).collect(),
            // The modulator's own bound is unit magnitude, which reaches the
            // audio scaled like everything else.
            limit: TX_REAL_SCALE,
        }
    }

    /// Forget what went through, ready for a new over.
    pub(crate) fn reset(&mut self) {
        self.first.reset();
        self.again.iter_mut().for_each(Fir::reset);
    }

    /// How many samples of silence push the last of the signal out.
    pub(crate) fn tail_len(&self) -> usize {
        (1 + self.again.len()) * (TAPS - 1)
    }

    /// Filter in place. The output runs behind the input by 12 ms for every
    /// run through the filter: 12 ms on V2, 48 ms on V1.
    pub(crate) fn process(&mut self, audio: &mut [f32]) {
        for s in audio.iter_mut() {
            let mut y = self.first.push(*s, &self.taps);
            for fir in &mut self.again {
                let envelope = y.norm();
                if envelope > self.limit {
                    y *= self.limit / envelope;
                }
                y = fir.push(y.re, &self.taps);
            }
            *s = y.re;
        }
    }
}

/// The modified Bessel function of the first kind, order zero.
fn bessel_i0(x: f64) -> f64 {
    let (mut term, mut sum) = (1.0, 1.0);
    for k in 1..40 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The level coming out for a steady tone going in, in dB.
    fn gain_db(version: RadeVersion, hz: f64) -> f64 {
        let mut f = TxFilter::new(version);
        let mut x: Vec<f32> = (0..4_000)
            .map(|n| 0.1 * (std::f64::consts::TAU * hz * n as f64 / MODEM_RATE).sin() as f32)
            .collect();
        f.process(&mut x);
        // Past the filling of every filter in the chain.
        let settled = &x[f.tail_len()..];
        let rms = (settled.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / settled.len() as f64)
            .sqrt();
        20.0 * (rms / (0.1 / std::f64::consts::SQRT_2)).log10()
    }

    #[test]
    fn carriers_pass_flat_and_the_neighbours_are_stopped() {
        for (version, first, last) in
            [(RadeVersion::V1, 750.0, 2200.0), (RadeVersion::V2, 1062.5, 1875.0)]
        {
            for hz in [first, (first + last) / 2.0, last] {
                let g = gain_db(version, hz);
                assert!(g.abs() < 0.1, "{version:?}: {g:.2} dB at carrier {hz} Hz");
            }
            for hz in [first - 300.0, last + 300.0] {
                let g = gain_db(version, hz);
                assert!(g < -55.0, "{version:?}: only {g:.1} dB at {hz} Hz");
            }
        }
    }

    #[test]
    fn v1_peaks_stay_within_the_modulators_own_bound() {
        let mut f = TxFilter::new(RadeVersion::V1);
        // A square wave in the passband: filtering and all, far over the top.
        let mut x: Vec<f32> =
            (0..4_000).map(|n| if (n / 3) % 2 == 0 { 0.9 } else { -0.9 }).collect();
        f.process(&mut x);
        let peak = x.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        // Within about 1.5 dB: the last filter lets a little grow back.
        assert!(peak < 1.2 * TX_REAL_SCALE, "peak {peak}");
        assert!(peak > 0.3, "limited, not silenced");
    }

    #[test]
    fn the_tail_brings_out_what_the_delay_held_back() {
        let mut f = TxFilter::new(RadeVersion::V2);
        let mut x = vec![0.0f32; 10];
        x[9] = 0.4;
        f.process(&mut x);
        let mut tail = vec![0.0f32; f.tail_len()];
        f.process(&mut tail);
        // The impulse comes out half the filter's length later.
        let peak = (0..tail.len()).max_by(|&a, &b| tail[a].abs().total_cmp(&tail[b].abs()));
        assert_eq!(peak, Some(TAPS / 2 - 1));
        // And reset leaves nothing behind.
        f.reset();
        let mut silence = vec![0.0f32; TAPS];
        f.process(&mut silence);
        assert!(silence.iter().all(|&s| s == 0.0));
    }
}
