//! Simple linear resampler for interleaved f32 audio.

/// Linear-interpolating resampler that keeps fractional position state across calls.
pub struct LinearResampler {
    from_rate: u32,
    channels: usize,
    ratio: f64, // input frames per output frame
    pos: f64,   // position in the virtual stream [last, input...]
    last: Vec<f32>,
    has_last: bool,
}

impl LinearResampler {
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Self {
        Self {
            from_rate,
            channels,
            ratio: from_rate as f64 / to_rate as f64,
            pos: 0.0,
            last: vec![0.0; channels],
            has_last: false,
        }
    }

    pub fn input_rate(&self) -> u32 {
        self.from_rate
    }

    pub fn is_identity(&self) -> bool {
        (self.ratio - 1.0).abs() < f64::EPSILON
    }

    /// Resamples `input` (interleaved) and appends the result to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / ch;
        if frames == 0 {
            return;
        }
        // Virtual frame index 0 is `last`, indices 1..=frames are the input.
        if !self.has_last {
            self.last.copy_from_slice(&input[..ch]);
            self.has_last = true;
            self.pos = 1.0;
        }
        let sample_at = |idx: usize, c: usize| -> f32 {
            if idx == 0 {
                self.last[c]
            } else {
                input[(idx - 1) * ch + c]
            }
        };
        let max_pos = frames as f64; // last valid interpolation start index
        while self.pos < max_pos {
            let idx = self.pos.floor() as usize;
            let frac = (self.pos - idx as f64) as f32;
            for c in 0..ch {
                let a = sample_at(idx, c);
                let b = sample_at(idx + 1, c);
                out.push(a + (b - a) * frac);
            }
            self.pos += self.ratio;
        }
        self.last.copy_from_slice(&input[(frames - 1) * ch..]);
        self.pos -= frames as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_sample_count() {
        let mut r = LinearResampler::new(96000, 48000, 1);
        let input: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(100) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 500).abs() <= 2, "len={}", out.len());
        for w in out.windows(2) {
            assert!(w[1] > w[0], "not monotonic: {:?}", w);
        }
    }

    #[test]
    fn upsamples_stereo() {
        let mut r = LinearResampler::new(44100, 48000, 2);
        let input: Vec<f32> = (0..882).map(|i| (i / 2) as f32).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert!((out.len() as i64 / 2 - 480).abs() <= 2, "frames={}", out.len() / 2);
    }
}
