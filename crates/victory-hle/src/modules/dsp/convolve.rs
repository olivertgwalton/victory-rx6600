//! Uniformly partitioned convolution (overlap-save), as the DSP runs a
//! convolution stream: each block of `F` input samples per channel joins the
//! previous one, is transformed to `F` packed bins, and is kept in a delay
//! line of spectra; the output block is the last `F` samples of the inverse
//! transform of `Σ_k X[t-k] · H[k]`, over the stream's partitions `H[k]`
//! (each the transform of `F` impulse-response samples followed by `F`
//! zeros).
//!
//! Spectra are kept as separate real and imaginary arrays so the
//! partition multiply-accumulate vectorizes, and every buffer is made once.

use super::fft::{C, Real};

pub(crate) struct Convolver {
    frames: usize,
    real: Real,
    /// Impulse-response spectra per set: `partitions * frames` bins.
    sets: Vec<Split>,
    partitions: usize,
    channels: Vec<Channel>,
    /// Partition gains, one per partition per set (1.0 without an envelope).
    envelope: Vec<Vec<f32>>,
    acc: Split,
    bins: Vec<C>,
    time: Vec<f32>,
}

struct Split {
    re: Vec<f32>,
    im: Vec<f32>,
}

impl Split {
    fn zeroed(n: usize) -> Split {
        Split {
            re: vec![0.0; n],
            im: vec![0.0; n],
        }
    }
}

struct Channel {
    /// The previous input block and the current one.
    window: Vec<f32>,
    /// Input spectra, newest at `head`.
    spectra: Split,
    head: usize,
    set: usize,
}

impl Convolver {
    /// `ir` holds `partitions * frames` packed bins (as float pairs) per set.
    pub fn new(frames: usize, channels: usize, partitions: usize, ir: &[f32], two_sets: bool) -> Self {
        let per_set = partitions * frames;
        let sets: Vec<Split> = (0..if two_sets { 2 } else { 1 })
            .map(|s| {
                let pairs = &ir[s * 2 * per_set..(s + 1) * 2 * per_set];
                let (pairs, _) = pairs.as_chunks::<2>();
                Split {
                    re: pairs.iter().map(|p| p[0]).collect(),
                    im: pairs.iter().map(|p| p[1]).collect(),
                }
            })
            .collect();
        let envelope = vec![vec![1.0; partitions]; sets.len()];
        let channels = (0..channels)
            .map(|c| Channel {
                window: vec![0.0; 2 * frames],
                spectra: Split::zeroed(per_set),
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
            acc: Split::zeroed(frames),
            bins: vec![C::default(); frames],
            time: vec![0.0; 2 * frames],
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
        self.push(c, input);
        self.render(c, output, gain);
    }

    /// Takes channel `c`'s next input block.
    pub fn push(&mut self, c: usize, input: &[f32]) {
        let f = self.frames;
        let p = self.partitions;
        let Some(ch) = self.channels.get_mut(c) else {
            return;
        };
        ch.window.copy_within(f.., 0);
        ch.window[f..].copy_from_slice(&input[..f]);
        ch.head = (ch.head + p - 1) % p;
        self.real.forward(&ch.window, &mut self.bins);
        let newest = ch.head * f..(ch.head + 1) * f;
        for ((re, im), b) in ch.spectra.re[newest.clone()]
            .iter_mut()
            .zip(&mut ch.spectra.im[newest])
            .zip(&self.bins)
        {
            *re = b.re;
            *im = b.im;
        }
    }

    /// Channel `c`'s output block for the inputs pushed so far.
    pub fn render(&mut self, c: usize, output: &mut [f32], gain: f32) {
        let f = self.frames;
        let p = self.partitions;
        let Some(ch) = self.channels.get(c) else {
            return;
        };
        let h = &self.sets[ch.set];
        let env = &self.envelope[ch.set];
        let (acc_re, acc_im) = (&mut self.acc.re[..f], &mut self.acc.im[..f]);
        acc_re.fill(0.0);
        acc_im.fill(0.0);
        for (k, &g) in env.iter().enumerate() {
            if g == 0.0 {
                continue;
            }
            let x = ((ch.head + k) % p) * f;
            let (xr, xi) = (&ch.spectra.re[x..x + f], &ch.spectra.im[x..x + f]);
            let (hr, hi) = (&h.re[k * f..(k + 1) * f], &h.im[k * f..(k + 1) * f]);
            // Bin 0 packs two real terms: DC in the real part, Nyquist in
            // the imaginary one.
            let (dc, nyquist) = (acc_re[0] + g * xr[0] * hr[0], acc_im[0] + g * xi[0] * hi[0]);
            for b in 0..f {
                acc_re[b] += g * (xr[b] * hr[b] - xi[b] * hi[b]);
                acc_im[b] += g * (xr[b] * hi[b] + xi[b] * hr[b]);
            }
            acc_re[0] = dc;
            acc_im[0] = nyquist;
        }
        for (b, (re, im)) in self.bins.iter_mut().zip(acc_re.iter().zip(acc_im.iter())) {
            *b = C::new(*re, *im);
        }
        self.real.inverse(&mut self.bins, &mut self.time);
        for (o, &t) in output.iter_mut().zip(&self.time[f..]) {
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
