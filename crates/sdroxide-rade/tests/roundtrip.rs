//! End-to-end tests against the speech samples vendored with rade_c.
//!
//! The unit tests in the library cover the FFI contract; these cover the parts
//! that only show up once real audio flows: that a transmit-then-receive round
//! trip recovers the features that went in, and that the worker's ring
//! boundaries don't corrupt framing when the caller's chunk size has nothing to
//! do with `rade_nin()`.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use num_complex::Complex32;
use sdroxide_rade::{
    MODEM_RATE, RX_REAL_SCALE, Rade, RadeWorker, SPEECH_RATE, TX_REAL_SCALE, VocoderEnc,
    f32_to_i16, text, vocoder,
};

/// Callsign carried in the End-of-Over frame. Deliberately not a real one:
/// nothing here goes on the air, but a test fixture should not look like it
/// belongs to someone.
const TEST_CALL: &str = "AA1ZZZZZ";

/// The C library documents one context per process, and cargo runs the tests in
/// this binary on threads, so they take turns.
static LOCK: Mutex<()> = Mutex::new(());
fn exclusive() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn sample(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/rade_c/wav").join(name)
}

/// 16-bit mono WAV as `+/-1.0` floats, with its sample rate.
fn read_wav(path: &PathBuf) -> (Vec<f32>, f64) {
    let mut r =
        hound::WavReader::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let spec = r.spec();
    assert_eq!(spec.channels, 1);
    let s = r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect();
    (s, spec.sample_rate as f64)
}

/// Speech -> features -> modem IQ, the same path the transmitter takes, with
/// `call` in the End-of-Over frame — so a receiver of this signal has a
/// callsign to recover, exactly as an on-air over does.
fn modulate(rade: &mut Rade, speech: &[f32], call: &str) -> (Vec<f32>, Vec<Complex32>) {
    let mut enc = VocoderEnc::new().expect("encoder");
    let frame = enc.frame_size();
    let n_block = rade.n_features();
    let (mut feats, mut sent, mut iq, mut pcm) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());

    for chunk in speech.chunks_exact(frame) {
        pcm.clear();
        pcm.extend(chunk.iter().copied().map(f32_to_i16));
        enc.encode(&pcm, &mut feats).expect("encode");
        while feats.len() >= n_block {
            let block: Vec<f32> = feats.drain(..n_block).collect();
            rade.tx(&block, &mut iq).expect("tx");
            sent.extend_from_slice(&block);
        }
    }
    let mut bits = vec![0.0f32; rade.n_eoo_bits()];
    text::encode(call, &mut bits);
    rade.set_tx_eoo_bits(&bits).expect("set eoo bits");
    rade.tx_eoo(&mut iq).expect("eoo");
    (sent, iq)
}

/// Mean squared error between two feature streams at a given frame offset,
/// over the first 20 (cepstral) dimensions of each vector.
fn feature_mse(a: &[f32], b: &[f32], lag_frames: usize, n_frames: usize) -> f64 {
    let n = vocoder::n_features();
    let (mut sum, mut count) = (0.0f64, 0usize);
    for f in 0..n_frames {
        let ai = (f + lag_frames) * n;
        let bi = f * n;
        if ai + n > a.len() || bi + n > b.len() {
            break;
        }
        for d in 0..20 {
            let e = (a[ai + d] - b[bi + d]) as f64;
            sum += e * e;
            count += 1;
        }
    }
    if count == 0 { f64::INFINITY } else { sum / count as f64 }
}

#[test]
fn transmit_then_receive_recovers_the_features() {
    let _g = exclusive();
    let (audio, rate) = read_wav(&sample("david_vk5dgr.wav"));
    assert_eq!(rate, SPEECH_RATE, "sample is expected to be 16 kHz");

    let mut rade = Rade::open_v1().expect("open");
    let (sent, iq) = modulate(&mut rade, &audio, TEST_CALL);
    assert!(!sent.is_empty() && !iq.is_empty());
    drop(rade);

    // Receive the modem signal exactly as a real-valued SSB channel delivers
    // it: real part only, scaled the way rade_api.h specifies.
    let mut rade = Rade::open_v1().expect("reopen");
    let mut received = Vec::new();
    let mut feats = Vec::new();
    let mut eoo_bits = Vec::new();
    let mut pos = 0;
    let mut synced = false;
    while pos < iq.len() {
        let nin = rade.nin();
        let mut block = vec![Complex32::default(); nin];
        for (dst, src) in block.iter_mut().zip(&iq[pos..]) {
            // Out through the transmit scaling and back in through the receive
            // scaling — i.e. exactly what a loopback over the audio path does.
            *dst = Complex32::new(src.re * TX_REAL_SCALE * RX_REAL_SCALE, 0.0);
        }
        pos += nin;
        rade.rx(&block, &mut feats, &mut eoo_bits).expect("rx");
        synced |= rade.sync();
        received.extend_from_slice(&feats);
    }
    assert!(synced, "receiver never reached sync");

    let n = vocoder::n_features();
    let sent_frames = sent.len() / n;
    let recv_frames = received.len() / n;
    assert!(
        recv_frames * 100 >= sent_frames * 90,
        "recovered {recv_frames} of {sent_frames} feature frames"
    );

    // The modem has a fixed pipeline delay, so find it once and score there.
    let compare = 200.min(recv_frames.saturating_sub(20));
    let (best_lag, best_mse) = (0..60)
        .map(|lag| (lag, feature_mse(&sent, &received, lag, compare)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .expect("a best lag");

    // Calibrate against nonsense: the same comparison against a chunk of the
    // transmission 300 frames away, which is unrelated speech. An absolute
    // threshold on cepstral MSE would just be a magic number; this asks the
    // question that matters — is the recovered stream much closer to what was
    // sent than two arbitrary pieces of the same talker are to each other?
    let baseline = feature_mse(&sent, &received, best_lag + 300, compare);
    eprintln!(
        "best lag {best_lag} frames, feature MSE {best_mse:.4} (unaligned baseline {baseline:.4})"
    );
    assert!(
        best_mse < baseline / 4.0,
        "round-trip feature MSE {best_mse:.4} at lag {best_lag} is not much better \
         than the unaligned baseline {baseline:.4}"
    );
}

/// The transmit side of the worker, end to end: microphone in at the engine's
/// rate, modulated audio out at the same rate, decoded back by a fresh
/// receiver. Covers `set_tx`, the end-of-over frame and `tx_drained`.
#[test]
fn worker_transmits_an_over_that_decodes_back() {
    let _g = exclusive();
    const AUDIO_RATE: f64 = 48_000.0;
    let (audio, rate) = read_wav(&sample("david_vk5dgr.wav"));
    assert_eq!(rate, SPEECH_RATE);

    // Microphone audio arrives at the engine's rate, not the vocoder's.
    let mut mic = Vec::new();
    sdroxide_dsp::MonoResampler::new(SPEECH_RATE, AUDIO_RATE)
        .expect("upsampler")
        .push(&audio, &mut mic);

    let mut modem = Vec::new();
    {
        let mut worker = RadeWorker::new(AUDIO_RATE, AUDIO_RATE).expect("worker");
        // Identify ourselves in the End-of-Over frame, as a real over would.
        worker.set_callsign(TEST_CALL);
        worker.set_tx(true);

        // Feed and drain in 10 ms blocks, the granularity the engine uses.
        // Only the samples the worker actually produced are kept: `pop_tx`
        // pads to fill the buffer, which is what a sound card needs but would
        // splice silence into the signal we are about to decode.
        let block = (AUDIO_RATE / 100.0) as usize;
        let mut out = vec![0.0f32; block];
        // The worker modulates its whole backlog in one wake, so a deep one
        // comes back out as a single burst; keep it to ~100 ms, well inside
        // what a 10 ms drain can carry away.
        let max_backlog = block * 10;
        for chunk in mic.chunks(block) {
            // Nothing paces a file the way a sound card paces a microphone, so
            // follow the worker's actual backlog rather than sleeping a fixed
            // amount: the modulator is faster than real time, but *how much*
            // faster is a property of the machine, and on a busy one a fixed
            // guess lets the test outrun the worker and lose audio.
            while worker.tx_pending() > max_backlog {
                let n = worker.pop_tx(&mut out);
                modem.extend_from_slice(&out[..n]);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            worker.push_mic(chunk);
            let n = worker.pop_tx(&mut out);
            modem.extend_from_slice(&out[..n]);
        }
        worker.set_tx(false);
        for _ in 0..500 {
            let n = worker.pop_tx(&mut out);
            modem.extend_from_slice(&out[..n]);
            if worker.tx_drained() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(worker.tx_drained(), "transmit never drained after releasing");
        assert_eq!(worker.stats().dropped, 0);
    }

    // Half a second of silence on the end, because a real receiver never stops
    // delivering audio: the end-of-over frame is the last thing transmitted,
    // and without a tail to push it through the demodulator's pipeline it would
    // never come back out.
    modem.resize(modem.len() + AUDIO_RATE as usize / 2, 0.0);

    let peak = modem.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    assert!(peak > 0.05, "transmitted signal peaked at only {peak}");

    // Decode what we just transmitted with a fresh receiver.
    let mut down = Vec::new();
    sdroxide_dsp::MonoResampler::new(AUDIO_RATE, MODEM_RATE)
        .expect("downsampler")
        .push(&modem, &mut down);

    let mut rade = Rade::open_v1().expect("open");
    let mut feats = Vec::new();
    let mut eoo_bits = Vec::new();
    let (mut pos, mut valid, mut eoo, mut synced) = (0usize, 0usize, 0usize, false);
    let mut heard: Option<String> = None;
    while pos < down.len() {
        let nin = rade.nin();
        let mut blk = vec![Complex32::default(); nin];
        for (dst, &src) in blk.iter_mut().zip(&down[pos..]) {
            *dst = Complex32::new(src * RX_REAL_SCALE, 0.0);
        }
        pos += nin;
        let out = rade.rx(&blk, &mut feats, &mut eoo_bits).expect("rx");
        valid += usize::from(out.n_features > 0);
        synced |= rade.sync();
        if out.has_eoo {
            eoo += 1;
            assert_eq!(eoo_bits.len(), rade.n_eoo_bits(), "eoo bits must be handed back");
            heard = heard.or_else(|| sdroxide_rade::text::decode(&eoo_bits));
        } else {
            assert!(eoo_bits.is_empty(), "eoo bits must be empty without an end-of-over");
        }
    }
    eprintln!(
        "transmitted {:.1} s, {valid} valid frames, {eoo} eoo, heard {heard:?}",
        modem.len() as f64 / AUDIO_RATE
    );
    assert!(synced, "the transmitted over never decoded");
    assert!(valid > 50, "only {valid} frames decoded from our own transmission");
    assert!(eoo > 0, "the end-of-over frame was not transmitted");
    // The whole point of the text channel: our callsign survived modulation,
    // the audio path and demodulation.
    assert_eq!(
        heard.as_deref(),
        Some(TEST_CALL),
        "the callsign did not survive the round trip through the modem"
    );
}

/// The End-of-Over frame must get out whole, whatever the length of the over
/// leaves in the transmit resampler's last block, with the transmit filter and
/// without it, and a short silence must follow it. Its callsign is the proof of
/// the first.
#[test]
fn the_end_of_every_over_gets_out() {
    let _g = exclusive();
    const AUDIO_RATE: f64 = 48_000.0;
    /// 100 ms, give or take what the resampler's interpolation smears into it.
    const SILENCE: std::ops::RangeInclusive<usize> = 4_700..=4_900;
    let (audio, _) = read_wav(&sample("david_vk5dgr.wav"));
    let mut mic = Vec::new();
    sdroxide_dsp::MonoResampler::new(SPEECH_RATE, AUDIO_RATE)
        .expect("upsampler")
        .push(&audio, &mut mic);

    for filter in [false, true] {
        let mut worker = RadeWorker::new(AUDIO_RATE, AUDIO_RATE).expect("worker");
        worker.set_callsign(TEST_CALL);
        worker.set_tx_filter(filter);
        // Overs one modem frame apart each leave a different remainder; the
        // same worker, so a leftover would also spoil the next one.
        let overs: Vec<Vec<f32>> = (0..6)
            .map(|k| {
                worker.set_tx(true);
                // One sample at a time: a longer buffer can come back with
                // silence in the middle while the worker is still writing.
                let (mut one, mut modem) = ([0.0f32; 1], Vec::new());
                for chunk in mic[..(3 * 48_000 + k * 5_760)].chunks(480) {
                    while worker.tx_pending() > 4_800 {
                        while worker.pop_tx(&mut one) == 1 {
                            modem.push(one[0]);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    worker.push_mic(chunk);
                }
                worker.set_tx(false);
                for _ in 0..1_000 {
                    while worker.pop_tx(&mut one) == 1 {
                        modem.push(one[0]);
                    }
                    if worker.tx_drained() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert!(worker.tx_drained());
                modem
            })
            .collect();
        drop(worker);

        for (k, mut modem) in overs.into_iter().enumerate() {
            let silence = modem.iter().rev().take_while(|s| s.abs() < 1e-4).count();
            assert!(
                SILENCE.contains(&silence),
                "over {k}, filter {filter}: {silence} samples of silence at its end"
            );
            modem.resize(modem.len() + AUDIO_RATE as usize / 2, 0.0);
            let mut down = Vec::new();
            sdroxide_dsp::MonoResampler::new(AUDIO_RATE, MODEM_RATE)
                .expect("downsampler")
                .push(&modem, &mut down);
            let mut rade = Rade::open_v1().expect("open");
            let (mut feats, mut eoo, mut pos, mut heard) = (Vec::new(), Vec::new(), 0, None);
            while pos < down.len() {
                let nin = rade.nin();
                let mut blk = vec![Complex32::default(); nin];
                for (dst, &src) in blk.iter_mut().zip(&down[pos..]) {
                    *dst = Complex32::new(src * RX_REAL_SCALE, 0.0);
                }
                pos += nin;
                if rade.rx(&blk, &mut feats, &mut eoo).expect("rx").has_eoo {
                    heard = heard.or_else(|| text::decode(&eoo));
                }
            }
            assert_eq!(heard.as_deref(), Some(TEST_CALL), "over {k}, filter {filter}");
        }
    }
}

#[test]
fn worker_survives_chunk_sizes_unrelated_to_nin() {
    let _g = exclusive();
    let (audio, rate) = read_wav(&sample("david_vk5dgr.wav"));
    assert_eq!(rate, SPEECH_RATE);

    // Build a modem signal to play back, then drop the context so the worker
    // can open its own.
    let modem: Vec<f32> = {
        let mut rade = Rade::open_v1().expect("open");
        let (_, iq) = modulate(&mut rade, &audio, TEST_CALL);
        iq.iter().map(|z| z.re * TX_REAL_SCALE).collect()
    };

    const AUDIO_RATE: f64 = 48_000.0;
    let mut worker = RadeWorker::new(MODEM_RATE, AUDIO_RATE).expect("worker");
    let mut speech = Vec::new();

    // Deliberately awkward chunk sizes: neither divides `nin`, which sits near
    // 960 but moves as timing tracking pulls the clock.
    // Half a second of silence on the end, because a real receiver never stops
    // delivering audio: without it the final partial block — which carries the
    // end-of-over frame — would sit in the worker's buffer forever.
    let mut modem = modem;
    let modem_len = modem.len();
    modem.resize(modem_len + MODEM_RATE as usize / 2, 0.0);

    let sizes = [480usize, 1000, 137];
    // RadeWorker's receive ring holds one second of MODEM_RATE samples.
    const MAX_RX_BACKLOG: usize = 2_000;
    const MIN_RX_FREE: usize = MODEM_RATE as usize - MAX_RX_BACKLOG;

    let mut pos = 0;
    let mut i = 0;
    while pos < modem.len() {
        let n = sizes[i % sizes.len()].min(modem.len() - pos);
        i += 1;
        while worker.rx_free() < MIN_RX_FREE + n {
            worker.pop_rx(&mut speech);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        worker.push_rx(&modem[pos..pos + n]);
        pos += n;
        worker.pop_rx(&mut speech);
    }
    // Let the tail work through.
    let mut heard: Option<String> = None;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        worker.pop_rx(&mut speech);
        // The worker's own callsign channel, which is what the engine reports
        // to FreeDV Reporter and what the panel shows. Draining it here is the
        // coverage that was missing: the test proved an EOO was *detected* and
        // never that its callsign came back out, so a worker-side decode bug
        // would have passed.
        for t in worker.poll_text() {
            heard = heard.or(Some(t.call));
        }
        if worker.stats().eoo_count > 0 && heard.is_some() {
            break;
        }
    }
    worker.pop_rx(&mut speech);
    for t in worker.poll_text() {
        heard = heard.or(Some(t.call));
    }

    let stats = worker.stats();
    eprintln!(
        "speech {} samples ({:.1} s), sync {}, snr {:.1} dB, eoo {}, dropped {}",
        speech.len(),
        speech.len() as f64 / AUDIO_RATE,
        stats.sync,
        stats.snr_db,
        stats.eoo_count,
        stats.dropped
    );
    assert_eq!(stats.dropped, 0, "the test paced badly and lost samples");
    assert!(stats.eoo_count > 0, "end-of-over was never detected");
    assert_eq!(
        heard.as_deref(),
        Some(TEST_CALL),
        "the worker did not surface the callsign from the end-of-over frame, so \
         FreeDV Reporter would never be told who was heard"
    );

    // Roughly the input duration, allowing for acquisition and the vocoder's
    // warm-up at the start.
    let expected = modem_len as f64 / MODEM_RATE * AUDIO_RATE;
    assert!(
        speech.len() as f64 > expected * 0.85,
        "only {} of ~{expected:.0} expected speech samples",
        speech.len()
    );
    // Decoded speech should actually be speech, not silence.
    let peak = speech.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    assert!(peak > 0.05, "decoded speech peaked at only {peak}");
}

/// V2 end to end through the worker, both ways: microphone in, modulated audio
/// out, and a second worker that has to sync on it, produce speech and recover
/// the callsign from the data symbols. Then V1 again in the same process, as
/// the panel's switch does it.
#[test]
fn v2_over_decodes_back_with_its_callsign() {
    use sdroxide_rade::RadeVersion;
    use std::time::{Duration, Instant};

    let _g = exclusive();
    const AUDIO_RATE: f64 = 48_000.0;
    const CALL: &str = "AA1ZZZ";
    let (audio, rate) = read_wav(&sample("david_vk5dgr.wav"));
    assert_eq!(rate, SPEECH_RATE);
    let mut mic = Vec::new();
    sdroxide_dsp::MonoResampler::new(SPEECH_RATE, AUDIO_RATE)
        .expect("upsampler")
        .push(&audio, &mut mic);
    let block = (AUDIO_RATE / 100.0) as usize;

    let mut modem = Vec::new();
    {
        let mut worker =
            RadeWorker::with_version(RadeVersion::V2, AUDIO_RATE, AUDIO_RATE).expect("worker");
        worker.set_callsign(CALL);
        worker.set_tx(true);
        let mut out = vec![0.0f32; block];
        for chunk in mic.chunks(block) {
            while worker.tx_pending() > block * 10 {
                let n = worker.pop_tx(&mut out);
                modem.extend_from_slice(&out[..n]);
                std::thread::sleep(Duration::from_millis(1));
            }
            worker.push_mic(chunk);
            let n = worker.pop_tx(&mut out);
            modem.extend_from_slice(&out[..n]);
        }
        worker.set_tx(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.tx_drained() && Instant::now() < deadline {
            let n = worker.pop_tx(&mut out);
            modem.extend_from_slice(&out[..n]);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(worker.tx_drained(), "transmit never drained after releasing");
        assert_eq!(worker.stats().dropped, 0);
    }
    let sent_s = modem.len() as f64 / AUDIO_RATE;
    let speech_s = audio.len() as f64 / SPEECH_RATE;
    assert!((sent_s - speech_s).abs() < 1.0, "sent {sent_s:.1} s for {speech_s:.1} s of speech");
    modem.resize(modem.len() + AUDIO_RATE as usize, 0.0);

    let mut worker =
        RadeWorker::with_version(RadeVersion::V2, AUDIO_RATE, AUDIO_RATE).expect("worker");
    let (mut speech, mut calls, mut synced) = (Vec::new(), Vec::new(), false);
    for chunk in modem.chunks(block) {
        // A second of ring; stay well inside it, and give the worker time.
        while worker.rx_free() < AUDIO_RATE as usize / 2 {
            std::thread::sleep(Duration::from_millis(1));
        }
        worker.push_rx(chunk);
        std::thread::sleep(Duration::from_millis(1));
        synced |= worker.stats().sync;
        worker.pop_rx(&mut speech);
        calls.extend(worker.poll_text());
    }
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(5));
        worker.pop_rx(&mut speech);
        calls.extend(worker.poll_text());
    }
    let stats = worker.stats();
    eprintln!(
        "V2: {sent_s:.1} s sent, {:.1} s speech decoded, {} eoo, {} callsign reports",
        speech.len() as f64 / AUDIO_RATE,
        stats.eoo_count,
        calls.len()
    );
    assert!(synced, "the V2 over never synced");
    assert!(speech.len() as f64 / AUDIO_RATE > speech_s * 0.8, "too little speech decoded");
    assert!(speech.iter().any(|s| s.abs() > 0.05), "decoded speech is silent");
    assert!(stats.eoo_count > 0, "the end-of-over frame was not detected");
    assert!(!calls.is_empty(), "no callsign recovered");
    assert!(calls.iter().all(|c| c.call == CALL), "{calls:?}");
    assert_eq!(stats.dropped, 0);
    drop(worker);

    drop(Rade::open(RadeVersion::V1).expect("V1 after V2"));
    let v2 = Rade::open(RadeVersion::V2).expect("V2 after V1");
    assert_eq!(v2.n_eoo_bits(), 0);
}
