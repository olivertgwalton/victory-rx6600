//! Uniformly partitioned convolution (overlap-save), as the DSP runs a
//! convolution stream: each block of `F` input samples per channel joins the
//! previous one, is transformed to `F` packed bins, and is kept in a delay
//! line of spectra; the output block is the last `F` samples of the inverse
//! transform of `Σ_k X[t-k] · H[k]`, over the stream's partitions `H[k]`
//! (each the transform of `F` impulse-response samples followed by `F`
//! zeros).

use super::fft::{C, Real};

pub(crate) struct Convolver {
    frames: usize,
    real: Real,
    /// Impulse-response spectra, `partitions * frames` bins per set.
    sets: Vec<Vec<C>>,
    partitions: usize,
    channels: Vec<Channel>,
    /// Partition gains, one per partition per set (1.0 without an envelope).
    envelope: Vec<Vec<f32>>,
}

struct Channel {
    /// The previous input block and the current one.
    window: Vec<f32>,
    /// Input spectra, newest at `head`.
    spectra: Vec<C>,
    head: usize,
    set: usize,
}

impl Convolver {
    /// `ir` holds `partitions * frames` packed bins (as float pairs) per set.
    pub fn new(frames: usize, channels: usize, partitions: usize, ir: &[f32], two_sets: bool) -> Self {
        let per_set = partitions * frames;
        let sets: Vec<Vec<C>> = (0..if two_sets { 2 } else { 1 })
            .map(|s| {
                ir[s * 2 * per_set..(s + 1) * 2 * per_set]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&[re, im]| C::new(re, im))
                    .collect()
            })
            .collect();
        let envelope = vec![vec![1.0; partitions]; sets.len()];
        let channels = (0..channels)
            .map(|c| Channel {
                window: vec![0.0; 2 * frames],
                spectra: vec![C::default(); per_set],
                head: 0,
                set: if two_sets { c } else { 0 },
            })
            .collect();
        Convolver {
            frames,
            real: Real::new(frames),
            sets,
            partitions,
            channels,
            envelope,
        }
    }

    pub fn set_envelope(&mut self, set: usize, gains: &[f32]) {
        if let Some(e) = self.envelope.get_mut(set) {
            for (g, &v) in e.iter_mut().zip(gains) {
                *g = v;
            }
        }
    }

    /// One block of channel `c`: `input` in, `output` out (`frames` each).
    pub fn block(&mut self, c: usize, input: &[f32], output: &mut [f32], gain: f32) {
        let f = self.frames;
        let Some(ch) = self.channels.get_mut(c) else {
            return;
        };
        ch.window.copy_within(f.., 0);
        ch.window[f..].copy_from_slice(&input[..f]);
        ch.head = (ch.head + self.partitions - 1) % self.partitions;
        let newest = &mut ch.spectra[ch.head * f..(ch.head + 1) * f];
        self.real.forward(&ch.window, newest);
        let h = &self.sets[ch.set];
        let env = &self.envelope[ch.set];
        let mut acc = vec![C::default(); f];
        for k in 0..self.partitions {
            let g = env[k];
            if g == 0.0 {
                continue;
            }
            let x = &ch.spectra[((ch.head + k) % self.partitions) * f..][..f];
            let hk = &h[k * f..(k + 1) * f];
            // Bin 0 packs two real terms.
            acc[0].re += g * x[0].re * hk[0].re;
            acc[0].im += g * x[0].im * hk[0].im;
            for b in 1..f {
                let p = x[b].mul(hk[b]);
                acc[b].re += g * p.re;
                acc[b].im += g * p.im;
            }
        }
        let mut time = vec![0f32; 2 * f];
        self.real.inverse(&acc, &mut time);
        for (o, &t) in output.iter_mut().zip(&time[f..]) {
            *o = t * gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct convolution of a signal with an impulse response matches the
    /// partitioned one, block for block.
    #[test]
    fn matches_direct_convolution() {
        let f = 8;
        let partitions = 3;
        let ir: Vec<f32> = (0..f * partitions).map(|i| 1.0 / (1 + i) as f32).collect();
        let real = Real::new(f);
        let mut spectra = vec![0f32; 2 * f * partitions];
        for k in 0..partitions {
            let mut padded = vec![0f32; 2 * f];
            padded[..f].copy_from_slice(&ir[k * f..(k + 1) * f]);
            let mut bins = vec![C::default(); f];
            real.forward(&padded, &mut bins);
            for (b, c) in bins.iter().enumerate() {
                spectra[2 * (k * f + b)] = c.re;
                spectra[2 * (k * f + b) + 1] = c.im;
            }
        }
        let mut conv = Convolver::new(f, 1, partitions, &spectra, false);
        let signal: Vec<f32> = (0..f * 6).map(|i| ((i * 5) % 7) as f32 - 3.0).collect();
        let mut out = vec![0f32; signal.len()];
        for (blk, o) in signal.chunks(f).zip(out.chunks_mut(f)) {
            conv.block(0, blk, o, 1.0);
        }
        for n in 0..signal.len() {
            let direct: f32 = (0..=n.min(ir.len() - 1)).map(|k| ir[k] * signal[n - k]).sum();
            assert!((direct - out[n]).abs() < 1e-3, "sample {n}: {direct} vs {}", out[n]);
        }
    }
}
