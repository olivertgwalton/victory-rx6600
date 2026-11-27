//! The real transforms of the DSP's convolutions: a real signal of `2F`
//! samples and its `F` packed bins (bin 0 holds the DC term and, as its
//! imaginary part, the Nyquist term), forward with `e^(-2πi kn/N)`, inverse
//! scaled by `1/N`. Built on an `F`-point complex transform, with the same
//! split twiddles `cos, -sin (2πk/N)` the console's DSP is given.

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(crate) struct C {
    pub re: f32,
    pub im: f32,
}

impl C {
    pub const fn new(re: f32, im: f32) -> C {
        C { re, im }
    }
    fn add(self, o: C) -> C {
        C::new(self.re + o.re, self.im + o.im)
    }
    fn sub(self, o: C) -> C {
        C::new(self.re - o.re, self.im - o.im)
    }
    pub fn mul(self, o: C) -> C {
        C::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }
    fn conj(self) -> C {
        C::new(self.re, -self.im)
    }
    fn scale(self, s: f32) -> C {
        C::new(self.re * s, self.im * s)
    }
}

/// Transforms of one size: `bins` packed bins, `2 * bins` real samples.
pub(crate) struct Real {
    bins: usize,
    /// `e^(-2πi k/bins)` for the complex transform.
    twiddles: Vec<C>,
    /// `e^(-2πi k/(2 bins))` for the split.
    split: Vec<C>,
    reversed: Vec<u32>,
}

impl Real {
    pub fn new(bins: usize) -> Real {
        assert!(bins.is_power_of_two() && bins >= 2);
        let angle = |k: usize, n: usize| -2.0 * std::f64::consts::PI * k as f64 / n as f64;
        let twiddles = (0..bins / 2)
            .map(|k| {
                let a = angle(k, bins);
                C::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let split = (0..bins)
            .map(|k| {
                let a = angle(k, 2 * bins);
                C::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let bits = bins.trailing_zeros();
        let reversed = (0..bins as u32)
            .map(|i| i.reverse_bits() >> (32 - bits))
            .collect();
        Real {
            bins,
            twiddles,
            split,
            reversed,
        }
    }

    /// In-place complex transform, forward (`e^-`) or inverse (`e^+`,
    /// unscaled).
    fn complex(&self, data: &mut [C], inverse: bool) {
        let n = self.bins;
        for i in 0..n {
            let j = self.reversed[i] as usize;
            if j > i {
                data.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let step = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let mut w = self.twiddles[k * step];
                    if inverse {
                        w = w.conj();
                    }
                    let a = data[start + k];
                    let b = data[start + k + len / 2].mul(w);
                    data[start + k] = a.add(b);
                    data[start + k + len / 2] = a.sub(b);
                }
            }
            len *= 2;
        }
    }

    /// `2 * bins` samples to `bins` packed bins.
    pub fn forward(&self, input: &[f32], out: &mut [C]) {
        let n = self.bins;
        for (k, z) in out.iter_mut().enumerate().take(n) {
            *z = C::new(input[2 * k], input[2 * k + 1]);
        }
        self.complex(out, false);
        let z0 = out[0];
        let mut packed = vec![C::default(); n];
        packed[0] = C::new(z0.re + z0.im, z0.re - z0.im);
        for k in 1..n {
            let a = out[k];
            let b = out[n - k].conj();
            let even = a.add(b).scale(0.5);
            let odd = a.sub(b).scale(0.5);
            // odd / i, times the split twiddle.
            let odd = C::new(odd.im, -odd.re).mul(self.split[k]);
            packed[k] = even.add(odd);
        }
        out.copy_from_slice(&packed);
    }

    /// `bins` packed bins to `2 * bins` samples, scaled by `1 / (2 bins)`.
    pub fn inverse(&self, input: &[C], out: &mut [f32]) {
        let n = self.bins;
        let mut z = vec![C::default(); n];
        let x0 = input[0];
        z[0] = C::new(x0.re + x0.im, x0.re - x0.im).scale(0.5);
        for k in 1..n {
            let a = input[k];
            let b = input[n - k].conj();
            let even = a.add(b).scale(0.5);
            let odd = a.sub(b).scale(0.5).mul(self.split[k].conj());
            // even + i * odd
            z[k] = even.add(C::new(-odd.im, odd.re));
        }
        self.complex(&mut z, true);
        let s = 1.0 / n as f32;
        for (k, v) in z.iter().enumerate() {
            out[2 * k] = v.re * s;
            out[2 * k + 1] = v.im * s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_a_plain_dft_and_round_trips() {
        let bins = 16;
        let real = Real::new(bins);
        let x: Vec<f32> = (0..2 * bins).map(|i| ((i * 7 + 3) % 11) as f32 - 5.0).collect();
        let mut spectrum = vec![C::default(); bins];
        real.forward(&x, &mut spectrum);
        let n = 2 * bins;
        let dft = |k: usize| {
            let (mut re, mut im) = (0f64, 0f64);
            for (t, &v) in x.iter().enumerate() {
                let a = -2.0 * std::f64::consts::PI * (k * t) as f64 / n as f64;
                re += v as f64 * a.cos();
                im += v as f64 * a.sin();
            }
            (re, im)
        };
        assert!((spectrum[0].re as f64 - dft(0).0).abs() < 1e-3);
        assert!((spectrum[0].im as f64 - dft(bins).0).abs() < 1e-3);
        for k in 1..bins {
            let (re, im) = dft(k);
            assert!((spectrum[k].re as f64 - re).abs() < 1e-3, "bin {k}");
            assert!((spectrum[k].im as f64 - im).abs() < 1e-3, "bin {k}");
        }
        let mut back = vec![0f32; n];
        real.inverse(&spectrum, &mut back);
        for (a, b) in x.iter().zip(&back) {
            assert!((a - b).abs() < 1e-4);
        }
    }
}
