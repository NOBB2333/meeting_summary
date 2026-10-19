//! Real-time microphone cleanup, shared by the recorder and the idle level meter.
//!
//! The chain stays small enough to run inline on every capture buffer without
//! allocating: a high-pass removes DC offset and low-frequency
//! rumble, an adaptive noise-floor tracker drives a smooth downward expander,
//! and a gentle compressor lifts speech the way conferencing apps do. Nothing is
//! ever hard muted, so quiet speech is not chopped into silence.

const FLOOR_MIN: f32 = 1e-7;
const FLOOR_MAX: f32 = 0.25;

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Coefficient for a one-pole response reaching ~63% of a step in `seconds`.
fn one_pole(seconds: f32, sample_rate: f32) -> f32 {
    1.0 - (-1.0 / (seconds * sample_rate)).exp()
}

#[derive(Clone, Copy, Default)]
struct Section {
    x1: f32,
    y1: f32,
}

impl Section {
    fn process(&mut self, input: f32, a: f32) -> f32 {
        let output = a * (self.y1 + input - self.x1);
        self.x1 = input;
        self.y1 = output;
        output
    }
}

/// Minimal complex number for the local radix-2 FFT.
#[derive(Clone, Copy, Debug, Default)]
struct Complex32 {
    re: f32,
    im: f32,
}

impl Complex32 {
    const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
    fn norm(self) -> f32 {
        (self.re * self.re + self.im * self.im).sqrt()
    }
}
/// Radix-2 in-place FFT. Input length must be a power of two.
fn fft(values: &mut [Complex32]) {
    let n = values.len();
    debug_assert!(n.is_power_of_two());
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            values.swap(i, j);
        }
    }
    let mut len = 2usize;
    while len <= n {
        let angle = -std::f32::consts::TAU / len as f32;
        let (step_re, step_im) = (angle.cos(), angle.sin());
        let mut start = 0usize;
        while start < n {
            let mut w_re = 1.0f32;
            let mut w_im = 0.0f32;
            for k in 0..len / 2 {
                let a = values[start + k];
                let b = values[start + k + len / 2];
                let t_re = w_re * b.re - w_im * b.im;
                let t_im = w_re * b.im + w_im * b.re;
                values[start + k] = Complex32::new(a.re + t_re, a.im + t_im);
                values[start + k + len / 2] = Complex32::new(a.re - t_re, a.im - t_im);
                let next_re = w_re * step_re - w_im * step_im;
                w_im = w_re * step_im + w_im * step_re;
                w_re = next_re;
            }
            start += len;
        }
        len <<= 1;
    }
}

fn fft_inverse(values: &mut [Complex32]) {
    for value in values.iter_mut() {
        value.im = -value.im;
    }
    fft(values);
    let n = values.len() as f32;
    for value in values {
        value.re /= n;
        value.im = -value.im / n;
    }
}

/// Square-root Hann analysis/synthesis, with 50% overlap, has unity gain.
const FFT_SIZE: usize = 512;
const FFT_HOP: usize = FFT_SIZE / 2;
const SPECTRAL_BINS: usize = FFT_SIZE / 2 + 1;

/// Per-channel STFT state. Allocation-free during normal processing.
struct SpectralChannel {
    input: Vec<f32>,
    output: Vec<f32>,
    window: Vec<f32>,
    ring: Vec<f32>,
    spectrum: Vec<Complex32>,
    min_mag: Vec<f32>,
    gain: Vec<f32>,
    frames: u64,
    overlap: Vec<f32>,
}

impl SpectralChannel {
    fn new() -> Self {
        Self {
            input: vec![0.0; FFT_HOP],
            output: vec![0.0; FFT_HOP],
            window: vec![0.0; FFT_SIZE],
            ring: vec![0.0; FFT_SIZE],
            spectrum: vec![Complex32::new(0.0, 0.0); FFT_SIZE],
            min_mag: vec![0.0; SPECTRAL_BINS],
            gain: vec![1.0; SPECTRAL_BINS],
            frames: 0,
            overlap: vec![0.0; FFT_HOP],
        }
    }
}

/// Hann-windowed spectral subtraction. It learns the stationary magnitude floor
/// per frequency bin and applies a Wiener gain. This removes the broadband hiss
/// that a time-domain gate cannot touch while leaving speech formants intact.
struct SpectralSuppressor {
    window: Vec<f32>,
    min_rise: f32,
    min_fall: f32,
    gain_rise: f32,
    gain_fall: f32,
    floor_gain: f32,
    channels: [SpectralChannel; 2],
    position: usize,
}

impl SpectralSuppressor {
    fn new(sample_rate: f32) -> Self {
        let hop_seconds = FFT_HOP as f32 / sample_rate.max(8_000.0);
        Self {
            window: (0..FFT_SIZE)
                .map(|i| {
                    (0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_SIZE as f32).cos()).sqrt()
                })
                .collect(),
            // Minimum-statistics tracking: the floor drops quickly onto quiet
            // frames and rises only very slowly, so stationary noise is learned
            // while speech and tones are treated as signal.
            min_rise: 1.0 - (-hop_seconds / 5.0).exp(),
            min_fall: 1.0 - (-hop_seconds / 0.02).exp(),
            gain_rise: 1.0 - (-hop_seconds / 0.01).exp(),
            gain_fall: 1.0 - (-hop_seconds / 0.08).exp(),
            floor_gain: db_to_linear(-14.0),
            channels: [SpectralChannel::new(), SpectralChannel::new()],
            position: 0,
        }
    }

    fn process(&mut self, samples: &mut [f32]) {
        for channel in 0..2 {
            self.channels[channel].window.copy_from_slice(&self.window);
        }
        // Read and write one stereo frame at a time. No raw samples escape at
        // packet boundaries, including devices delivering 441 or 480 frames.
        for frame in samples.chunks_exact_mut(2) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let data = &mut self.channels[channel];
                data.input[self.position] = if sample.is_finite() { *sample } else { 0.0 };
                *sample = data.output[self.position];
            }
            self.position += 1;
            if self.position == FFT_HOP {
                self.transform();
                self.position = 0;
            }
        }
    }

    fn transform(&mut self) {
        for channel in 0..2 {
            let data = &mut self.channels[channel];
            // Slide the analysis window by one hop: the previous second half is
            // reused verbatim, and the freshly captured hop fills the tail.
            data.ring.copy_within(FFT_HOP..FFT_SIZE, 0);
            for i in 0..FFT_HOP {
                data.ring[FFT_HOP + i] = data.input[i];
            }

            for i in 0..FFT_SIZE {
                data.spectrum[i] = Complex32::new(data.ring[i] * data.window[i], 0.0);
            }
            fft(&mut data.spectrum);

            for k in 0..SPECTRAL_BINS {
                let measured = data.spectrum[k].norm();
                let magnitude = if measured.is_finite() { measured } else { 0.0 };
                if data.frames == 0 {
                    // Start conservatively, without learning the first spoken
                    // syllable as the noise floor.
                    data.min_mag[k] = magnitude * 0.25;
                } else if magnitude < data.min_mag[k] {
                    data.min_mag[k] += (magnitude - data.min_mag[k]) * self.min_fall;
                } else {
                    data.min_mag[k] += (magnitude - data.min_mag[k]) * self.min_rise;
                }
                data.min_mag[k] = data.min_mag[k].max(FLOOR_MIN);
                let signal = magnitude * magnitude;
                // Conservative subtraction protects speech from a noise-floor
                // estimate that also contains some of the speaker's voice.
                let noise = 2.0 * data.min_mag[k] * data.min_mag[k];
                let wiener = if signal <= 1e-14 || !signal.is_finite() {
                    self.floor_gain
                } else {
                    let value = (signal - noise) / signal;
                    if value.is_finite() {
                        value.clamp(self.floor_gain, 1.0)
                    } else {
                        self.floor_gain
                    }
                };
                let coefficient = if wiener > data.gain[k] {
                    self.gain_rise
                } else {
                    self.gain_fall
                };
                data.gain[k] += (wiener - data.gain[k]) * coefficient;
                let gain = data.gain[k];
                let bin = &mut data.spectrum[k];
                bin.re *= gain;
                bin.im *= gain;
                if k > 0
                    && k < FFT_SIZE - k
                    && let Some(mirror) = data.spectrum.get_mut(FFT_SIZE - k)
                {
                    mirror.re *= gain;
                    mirror.im *= gain;
                }
            }
            fft_inverse(&mut data.spectrum);
            // Overlap adjacent synthesis windows, never the two halves of
            // the same window (that folds and corrupts the waveform).
            for i in 0..FFT_HOP {
                data.output[i] = data.overlap[i] + data.spectrum[i].re * data.window[i];
                data.overlap[i] = data.spectrum[FFT_HOP + i].re * data.window[FFT_HOP + i];
            }
            data.frames += 1;
        }
    }
}
/// Voice-oriented microphone conditioning for interleaved stereo buffers.
pub struct MicDenoiser {
    spectral: SpectralSuppressor,
    high_pass: [Section; 2],
    high_pass_a: f32,
    release: f32,
    gate_attack: f32,
    gate_release: f32,
    gate_hold_frames: u32,
    gate_hold_remaining: u32,
    comp_attack: f32,
    comp_release: f32,
    envelope: f32,
    floor: f32,
    warmup_remaining: usize,
    block_sum: f64,
    block_count: u32,
    block_len: u32,
    min_ring: Vec<f32>,
    min_index: usize,
    gate_gain: f32,
    comp_gain: f32,
    gate_close: f32,
    gate_open: f32,
    min_gain: f32,
    comp_threshold: f32,
    comp_slope: f32,
    makeup: f32,
    agc_gain: f32,
    agc_attack: f32,
    agc_release: f32,
    agc_min: f32,
    agc_max: f32,
    agc_target: f32,
}

impl MicDenoiser {
    pub fn new(sample_rate: u32) -> Self {
        let fs = sample_rate.max(8_000) as f32;
        let cutoff = 100.0;
        // 5 ms blocks over a 1.5 s sliding window give a stable noise-floor
        // estimate that speech transients cannot drag upwards.
        let block_len = ((fs * 0.005) as usize).max(1);
        let ring_len = ((fs * 1.5) / block_len as f32) as usize;
        Self {
            spectral: SpectralSuppressor::new(fs),
            high_pass: [Section::default(); 2],
            high_pass_a: 1.0 / (1.0 + std::f32::consts::TAU * cutoff / fs),
            release: one_pole(0.060, fs),
            gate_attack: one_pole(0.004, fs),
            gate_release: one_pole(0.450, fs),
            gate_hold_frames: (fs * 0.20) as u32,
            gate_hold_remaining: 0,
            comp_attack: one_pole(0.005, fs),
            comp_release: one_pole(0.200, fs),
            envelope: 0.0,
            floor: 0.0,
            warmup_remaining: FFT_SIZE * 3,
            block_sum: 0.0,
            block_count: 0,
            block_len: block_len as u32,
            min_ring: vec![f32::INFINITY; ring_len.max(1)],
            min_index: 0,
            gate_gain: 1.0,
            comp_gain: 1.0,
            gate_close: db_to_linear(3.5),
            gate_open: db_to_linear(9.0),
            min_gain: db_to_linear(-15.0),
            comp_threshold: db_to_linear(-18.0),
            comp_slope: 1.0 - 1.0 / 3.0,
            makeup: db_to_linear(3.0),
            agc_gain: 1.0,
            agc_attack: one_pole(0.10, fs),
            agc_release: one_pole(2.0, fs),
            agc_min: db_to_linear(-18.0),
            agc_max: db_to_linear(18.0),
            agc_target: db_to_linear(-17.0),
        }
    }

    pub const LATENCY_FRAMES: usize = FFT_SIZE;

    /// Tracked noise floor as a linear amplitude, exposed for diagnostics.
    pub fn floor_amplitude(&self) -> f32 {
        self.floor
    }

    /// Current gate gain, exposed for diagnostics and tests.
    pub fn gate_gain(&self) -> f32 {
        self.gate_gain
    }

    /// In-place processing of interleaved stereo frames (`[l, r, l, r, ...]`).
    pub fn process(&mut self, samples: &mut [f32]) {
        // Remove DC before the spectral tracker, so it cannot learn a large DC
        // bin and turn a steady offset into a modulated audible signal.
        for frame in samples.chunks_exact_mut(2) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let input = if sample.is_finite() { *sample } else { 0.0 };
                *sample = self.high_pass[channel].process(input, self.high_pass_a);
            }
        }
        self.spectral.process(samples);
        for frame in samples.chunks_exact_mut(2) {
            let mut envelope_input = 0.0f32;
            for sample in frame.iter_mut() {
                if !sample.is_finite() {
                    *sample = 0.0;
                }
                envelope_input = envelope_input.max(sample.abs());
            }
            // Instant attack so transients are never clipped; slow release so
            // natural pauses keep their gain instead of chattering.
            self.envelope = if envelope_input > self.envelope {
                envelope_input
            } else {
                self.envelope + (envelope_input - self.envelope) * self.release
            };

            // The overlap-add startup contains synthetic zeros, not a quiet
            // room. Learning them would treat background noise as speech for
            // the full minimum-statistics window and amplify it through AGC.
            if self.warmup_remaining > 0 {
                self.warmup_remaining -= 1;
            } else {
                self.block_sum += f64::from(self.envelope);
                self.block_count += 1;
            }
            if self.block_count >= self.block_len {
                let average = (self.block_sum / f64::from(self.block_count)) as f32;
                self.block_sum = 0.0;
                self.block_count = 0;
                self.min_ring[self.min_index] = average;
                self.min_index = (self.min_index + 1) % self.min_ring.len();
                let mut minimum = f32::INFINITY;
                for value in &self.min_ring {
                    minimum = minimum.min(*value);
                }
                self.floor = minimum.clamp(FLOOR_MIN, FLOOR_MAX);
            }

            let close = self.floor * self.gate_close;
            let open = self.floor * self.gate_open;
            // Preserve room tone through short pauses. The spectral stage
            // already removes steady noise; another deep, fast gate audibly
            // switches the room on and off at every syllable.
            let speech = self.envelope >= open && self.floor > 0.0;
            if speech {
                self.gate_hold_remaining = self.gate_hold_frames;
            } else {
                self.gate_hold_remaining = self.gate_hold_remaining.saturating_sub(1);
            }
            let target = if speech || self.gate_hold_remaining > 0 {
                1.0
            } else if self.envelope <= close {
                self.min_gain
            } else {
                let span = (open - close).max(1e-9);
                let t = ((self.envelope - close) / span).clamp(0.0, 1.0);
                let smooth = t * t * (3.0 - 2.0 * t);
                self.min_gain + (1.0 - self.min_gain) * smooth
            };
            let gate_coefficient = if target > self.gate_gain {
                self.gate_attack
            } else {
                self.gate_release
            };
            self.gate_gain += (target - self.gate_gain) * gate_coefficient;

            let level = self.envelope * self.gate_gain;
            let compression = if level > self.comp_threshold {
                (self.comp_threshold / level).powf(self.comp_slope)
            } else {
                1.0
            };
            let comp_coefficient = if compression < self.comp_gain {
                self.comp_attack
            } else {
                self.comp_release
            };
            self.comp_gain += (compression - self.comp_gain) * comp_coefficient;

            // Slow speech-level normalisation, the way conferencing clients keep a
            // quiet array microphone audible. Gain rises on a ~2 s release so room
            // noise is not pumped up, and falls on ~100 ms so shouts never clip.
            if speech {
                let desired = (self.agc_target / level.max(1e-6)).clamp(self.agc_min, self.agc_max);
                let coefficient = if desired < self.agc_gain {
                    self.agc_attack
                } else {
                    self.agc_release
                };
                self.agc_gain += (desired - self.agc_gain) * coefficient;
            }
            let gain = self.gate_gain * self.comp_gain * self.makeup * self.agc_gain;
            for sample in frame.iter_mut() {
                *sample = (*sample * gain).clamp(-1.0, 1.0);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32).sqrt()
    }

    fn interleave(values: impl Iterator<Item = f32>) -> Vec<f32> {
        values.flat_map(|v| [v, v]).collect()
    }

    /// Deterministic pseudo-random noise, so tests need no generator crate.
    fn noise_sample(state: &mut u32, amplitude: f32) -> f32 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let unit = (*state >> 8) as f32 / (1 << 24) as f32;
        (unit * 2.0 - 1.0) * amplitude
    }

    fn noise(seconds: f32, amplitude: f32) -> Vec<f32> {
        let mut state = 0x1234_5678u32;
        let count = (RATE * seconds) as usize;
        interleave((0..count).map(|_| noise_sample(&mut state, amplitude)))
    }

    /// Syllable-rate speech stand-in: tone bursts over a quiet noise bed.
    fn speech_bursts(seconds: f32, amplitude: f32, bed: f32) -> Vec<f32> {
        let mut state = 0x9e37_79b9u32;
        let count = (RATE * seconds) as usize;
        let period = (RATE * 0.32) as usize;
        let on = (RATE * 0.20) as usize;
        interleave((0..count).map(|i| {
            let bed = noise_sample(&mut state, bed);
            if i % period < on {
                let position = (i % period) as f32;
                let shape = (std::f32::consts::PI * position / on as f32).sin();
                amplitude * shape * (std::f32::consts::TAU * 220.0 * i as f32 / RATE).sin() + bed
            } else {
                bed
            }
        }))
    }

    #[test]
    fn spectral_round_trip_scaling_is_unity_for_a_constant() {
        let frames = 48_000;
        let source = interleave(std::iter::repeat_n(0.25f32, frames));
        let mut samples = source.clone();
        let mut suppressor = SpectralSuppressor::new(RATE);
        suppressor.floor_gain = 1.0; // Test reconstruction independently of suppression.
        for block in samples.chunks_mut(960) {
            suppressor.process(block);
        }
        let tail = &samples[samples.len() - 9_600..];
        let value = tail.iter().sum::<f32>() / tail.len() as f32;
        println!("constant round-trip value: {value:.5}");
        assert!(
            (value - 0.25).abs() < 0.01,
            "unity gain broken, got {value:.5}"
        );
    }

    #[test]
    fn spectral_reconstruction_preserves_both_channels_and_the_final_samples() {
        let source: Vec<f32> = (0..1031)
            .flat_map(|i| {
                let left = (i as f32 * 0.037).sin() * 0.3;
                let right = if i == 1030 {
                    0.7
                } else {
                    (i as f32 * 0.113).cos() * 0.2
                };
                [left, right]
            })
            .collect();
        let mut output = source.clone();
        output.extend(std::iter::repeat_n(0.0, MicDenoiser::LATENCY_FRAMES * 2));
        let mut suppressor = SpectralSuppressor::new(RATE);
        suppressor.floor_gain = 1.0;
        for packet in output.chunks_mut(441 * 2) {
            suppressor.process(packet);
        }
        let delay = MicDenoiser::LATENCY_FRAMES * 2;
        assert!(output[..delay].iter().all(|v| v.abs() < 0.00002));
        for (index, (actual, expected)) in output[delay..].iter().zip(&source).enumerate() {
            assert!(
                (actual - expected).abs() < 0.00002,
                "sample {index} was shifted, folded, or lost: {actual} vs {expected}"
            );
        }
    }

    #[test]
    fn spectral_stage_suppresses_steady_hiss_more_than_a_gate_could() {
        let raw = noise(4.0, 0.03);
        let raw_rms = rms(&raw[raw.len() - 48_000..]);
        let mut denoiser = MicDenoiser::new(48_000);
        let mut samples = raw;
        denoiser.process(&mut samples);
        let tail = &samples[samples.len() - 48_000..];
        let measured = 20.0 * (rms(tail) / raw_rms).log10();
        assert!(
            measured < -12.0,
            "steady hiss only dropped {measured:.1} dB; expected at least -12 dB with conservative speech-preserving suppression"
        );
    }

    #[test]
    fn gate_and_spectral_stage_push_a_quiet_tail_below_the_noise_floor() {
        // Speech, then a genuine silence so the release has something to work on.
        let mut source = speech_bursts(3.0, 0.18, 0.02);
        let tail = 48_000usize;
        source.extend(std::iter::repeat_n(0.0f32, tail * 2));
        let quiet_start = source.len() - tail * 2;
        let raw_bed = rms(&source[quiet_start + 24_000..]).max(1e-6);
        let mut samples = source.clone();
        let mut denoiser = MicDenoiser::new(48_000);
        denoiser.process(&mut samples);
        let processed_bed = rms(&samples[quiet_start + 24_000..]);
        let bed_db = 20.0 * (processed_bed / raw_bed).log10();
        assert!(
            processed_bed < raw_bed * 0.5,
            "quiet tail was not attenuated: {raw_bed:.5} -> {processed_bed:.5} ({bed_db:.1} dB)"
        );
        assert!(
            processed_bed < 0.02,
            "quiet tail leaked at {processed_bed:.5}"
        );
    }

    #[test]
    fn continuous_room_tone_survives_pauses_without_agc_chasing_the_noise() {
        let mut source = noise(2.0, 0.003);
        let mut voice = interleave(
            (0..48_000).map(|i| (std::f32::consts::TAU * 440. * i as f32 / RATE).sin() * 0.1),
        );
        let bed = noise(1.0, 0.003);
        for (v, n) in voice.iter_mut().zip(bed) {
            *v += n;
        }
        source.extend(voice.clone());
        let pause_start = source.len();
        source.extend(noise(2.0, 0.003));
        let resume = source.len();
        source.extend(voice);
        let mut denoiser = MicDenoiser::new(48_000);
        let mut output = source.clone();
        let mut during_pause = Vec::new();
        for (index, packet) in output.chunks_mut(960).enumerate() {
            denoiser.process(packet);
            let offset = index * 960;
            if offset >= pause_start + 19_200 && offset < resume {
                during_pause.push((denoiser.gate_gain(), denoiser.agc_gain));
            }
        }
        // The first 200 ms keeps the room open; no gate chop between clauses.
        assert!(
            during_pause[0].0 > 0.9,
            "short pause closed the room abruptly: {:?}",
            during_pause[0]
        );
        let agc_start = during_pause[20].1;
        let agc_end = during_pause.last().unwrap().1;
        assert!(
            (agc_end / agc_start - 1.).abs() < 0.02,
            "AGC amplified silence: {agc_start} -> {agc_end}"
        );
        // Twenty-millisecond windows of an actual nonzero noise bed remain
        // audible rather than collapsing by tens of dB at a gate boundary.
        let windows: Vec<_> = output[pause_start + 48_000..resume]
            .chunks_exact(1920)
            .map(rms)
            .collect();
        assert!(windows.iter().all(|level| *level > 0.00001));
        for pair in windows.windows(2) {
            let change = 20. * (pair[1] / pair[0]).log10();
            assert!(
                change.abs() < 4.,
                "room tone stepped by {change:.1} dB in 20 ms"
            );
        }
        let delay = MicDenoiser::LATENCY_FRAMES * 2;
        let onset = rms(&output[resume + delay..resume + delay + 3840]);
        let established = rms(&output[resume + delay + 9600..resume + delay + 13440]);
        assert!(
            onset > established * 0.65,
            "speech onset was swallowed: {onset} vs {established}"
        );
    }
    #[test]
    fn device_packet_sizes_do_not_change_the_recorded_signal() {
        let source = speech_bursts(1.0, 0.12, 0.004);
        let mut whole = source.clone();
        MicDenoiser::new(48_000).process(&mut whole);
        for frames in [128, 240, 441, 480, 512, 960] {
            let mut fragmented = source.clone();
            let mut denoiser = MicDenoiser::new(48_000);
            for packet in fragmented.chunks_mut(frames * 2) {
                denoiser.process(packet);
            }
            let difference = whole
                .iter()
                .zip(&fragmented)
                .fold(0.0f32, |max, (a, b)| max.max((a - b).abs()));
            assert!(
                difference < 1e-5,
                "packet size {frames} changes signal by {difference}"
            );
        }
    }

    #[test]
    fn removes_dc_offset_and_non_finite_values() {
        let mut denoiser = MicDenoiser::new(48_000);
        let mut samples = interleave(std::iter::repeat_n(0.3, 72_000));
        samples.push(f32::NAN);
        samples.push(f32::INFINITY);
        denoiser.process(&mut samples);
        let tail = &samples[samples.len() - 19_200..];
        assert!(tail.iter().all(|v| v.is_finite()));
        assert!(rms(tail) < 0.01, "DC offset survived: {}", rms(tail));
    }

    #[test]
    fn agc_lifts_a_quiet_voice_without_letting_it_clip() {
        let quiet = speech_bursts(4.0, 0.02, 0.002);
        let loud = speech_bursts(4.0, 0.6, 0.002);
        let mut quiet_out = quiet.clone();
        let mut loud_out = loud.clone();
        MicDenoiser::new(48_000).process(&mut quiet_out);
        MicDenoiser::new(48_000).process(&mut loud_out);
        let peak = |v: &[f32]| v.iter().fold(0.0f32, |p, x| p.max(x.abs()));
        let quiet_before = peak(&quiet);
        let quiet_after = peak(&quiet_out);
        let loud_after = peak(&loud_out);
        assert!(
            quiet_after > quiet_before * 2.0,
            "quiet speech was not lifted: {quiet_before:.4} -> {quiet_after:.4}"
        );
        assert!(
            loud_after < 1.0,
            "AGC let loud speech clip at {loud_after:.4}"
        );
    }
    #[test]
    fn attenuates_steady_noise_once_the_floor_is_learned() {
        let mut denoiser = MicDenoiser::new(48_000);
        let mut samples = noise(3.0, 0.02);
        denoiser.process(&mut samples);
        let tail = &samples[samples.len() - 24_000..];
        let attenuation_db = 20.0 * (rms(tail) / (0.02 / 3f32.sqrt())).log10();
        assert!(
            attenuation_db < -8.0,
            "expected the noise floor to be pushed down, got {attenuation_db} dB"
        );
        assert!(denoiser.floor_amplitude() < 0.05);
    }

    #[test]
    fn speech_bursts_keep_their_level_above_the_noise_bed() {
        let mut denoiser = MicDenoiser::new(48_000);
        let mut samples = speech_bursts(4.0, 0.2, 0.005);
        denoiser.process(&mut samples);
        // Compare the last full burst against the same span of raw input.
        let burst = 9_600;
        let start = samples.len() - burst;
        let ratio = rms(&samples[start..]) / rms(&speech_bursts(4.0, 0.2, 0.005)[start..]);
        assert!(ratio > 0.7, "speech was attenuated too much: {ratio}");
        assert!(rms(&samples[start..]) <= 1.0);
    }

    #[test]
    fn sustained_speech_does_not_raise_the_noise_floor() {
        let mut denoiser = MicDenoiser::new(48_000);
        let mut samples = speech_bursts(20.0, 0.25, 0.004);
        denoiser.process(&mut samples);
        assert!(
            denoiser.floor_amplitude() < 0.02,
            "floor drifted up to {}",
            denoiser.floor_amplitude()
        );
        let tail = &samples[samples.len() - 48_000..];
        assert!(rms(tail) > 0.05, "output went silent: {}", rms(tail));
    }

    #[test]
    fn output_stays_bounded_for_loud_input() {
        let mut denoiser = MicDenoiser::new(48_000);
        let mut samples: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let value = (std::f32::consts::TAU * 300.0 * i as f32 / RATE).sin();
                [value, value]
            })
            .collect();
        denoiser.process(&mut samples);
        assert!(samples.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
    }
}
