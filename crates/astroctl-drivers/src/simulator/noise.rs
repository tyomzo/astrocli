//! The deterministic random source every synthetic frame is built from.
//!
//! # Why this file exists instead of a dependency
//!
//! A simulator frame must be reproducible *forever*: the star field is seeded by the mount's
//! pointing (see [`sky`](super::sky)), and "the same sky comes back for the same pointing" is a
//! promise that a PRNG crate can break in a patch release without breaking its own semver — the
//! stream of numbers is not part of most RNG crates' API contract. A golden test written against
//! `rand` 0.8 and re-run under 0.9 is a test that fails for a reason nobody can act on.
//!
//! The workspace already reasons this way in the other direction: `Cargo.toml` picks `getrandom`
//! over `rand` for the ws-ticket because that use wants OS entropy and nothing else. This use
//! wants the exact opposite — a fixed stream, never any entropy — and thirty lines of
//! well-documented xoshiro is a smaller thing to own than a dependency whose value would be
//! features this must not use.
//!
//! # What is here
//!
//! [`Rng`] is xoshiro256++ seeded through SplitMix64 (Blackman & Vigna, public domain), plus the
//! two distributions a sensor needs: a Gaussian for read noise and seeing, and a Poisson for
//! photon shot noise. Both are standard algorithms, named at each site so the arithmetic can be
//! checked against a reference rather than believed.

/// Threshold above which [`Rng::poisson`] switches from Knuth's exact method to the Gaussian
/// approximation.
///
/// Knuth's method costs one multiply per event, i.e. `λ + 1` iterations on average: at a sky
/// level of 600 e⁻ per pixel — an ordinary 30 s sub — that is 600 multiplies **per pixel**, or
/// 14 billion for one 6000×4000 frame. The Gaussian approximation is within 1% of the Poisson
/// in both skewness and kurtosis by λ = 30 and costs one normal deviate, which is what makes a
/// full-size frame a second rather than a minute. Below the threshold the exact method runs,
/// because that is where the approximation is visibly wrong (it goes negative).
const POISSON_EXACT_BELOW: f64 = 30.0;

/// A reproducible pseudo-random stream.
///
/// Cheap to construct — construction is a seed expansion, not an entropy read — which is what
/// lets the frame generator make one per pixel band and per catalogue cell and get a stream that
/// depends only on which band and which cell.
#[derive(Debug, Clone)]
pub(super) struct Rng {
    state: [u64; 4],
    /// Box–Muller produces two independent normal deviates per transcendental pair; the second
    /// is kept here rather than thrown away, halving the cost of per-pixel read noise.
    spare_normal: Option<f64>,
}

impl Rng {
    /// Expands a 64-bit seed into a full xoshiro state with SplitMix64.
    ///
    /// Seeding xoshiro by copying the seed into the state is the documented way to get a bad
    /// stream out of a good generator: a state that is almost all zeros stays almost all zeros
    /// for thousands of outputs. SplitMix64 is the author's own recommended fix and is why a
    /// caller may pass a seed of 0, or two seeds one apart, and still get unrelated streams —
    /// which the catalogue relies on, since its seeds *are* consecutive cell indices.
    pub(super) fn seeded(seed: u64) -> Self {
        let mut z = seed;
        let mut next = || {
            z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^ (x >> 31)
        };
        Self {
            state: [next(), next(), next(), next()],
            spare_normal: None,
        }
    }

    /// Mixes several values into one seed, so a stream can be addressed by what it is *for*.
    ///
    /// Every seed in this driver is a tuple — (world seed, catalogue cell), (world seed, frame
    /// number, pixel band) — and hashing them here keeps the tuple visible at the call site
    /// instead of encoded in arithmetic nobody can read back.
    pub(super) fn stream(parts: &[u64]) -> Self {
        // FNV-1a over the 64-bit words. A cryptographic hash would be the wrong tool: this needs
        // to be fast and to avoid accidental structure, not to resist an adversary.
        let mut hash = 0xCBF2_9CE4_8422_2325_u64;
        for part in parts {
            hash ^= *part;
            hash = hash.wrapping_mul(0x1000_0000_01B3);
        }
        Self::seeded(hash)
    }

    /// The next 64 bits — xoshiro256++.
    pub(super) fn next_u64(&mut self) -> u64 {
        let result = self.state[0]
            .wrapping_add(self.state[3])
            .rotate_left(23)
            .wrapping_add(self.state[0]);
        let t = self.state[1] << 17;
        self.state[2] ^= self.state[0];
        self.state[3] ^= self.state[1];
        self.state[1] ^= self.state[2];
        self.state[0] ^= self.state[3];
        self.state[2] ^= t;
        self.state[3] = self.state[3].rotate_left(45);
        result
    }

    /// A uniform deviate in `[0, 1)`.
    ///
    /// The top 53 bits, which is the standard construction: taking the low bits instead would
    /// sample a generator's weakest bits, and xoshiro's lowest bit is a linear function of its
    /// state.
    pub(super) fn unit(&mut self) -> f64 {
        // 2^-53 exactly; the multiply is lossless.
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }

    /// A uniform deviate in `[low, high)`.
    pub(super) fn range(&mut self, low: f64, high: f64) -> f64 {
        low + self.unit() * (high - low)
    }

    /// A standard normal deviate — Box–Muller in polar form.
    ///
    /// The polar (Marsaglia) variant is used rather than the trigonometric one because it needs
    /// no `sin`/`cos`: read noise is drawn once per pixel, so 24 million of these are on the
    /// critical path of every full-size frame.
    pub(super) fn normal(&mut self) -> f64 {
        if let Some(spare) = self.spare_normal.take() {
            return spare;
        }
        loop {
            let x = self.range(-1.0, 1.0);
            let y = self.range(-1.0, 1.0);
            let square = x * x + y * y;
            // Rejection keeps the pair inside the unit circle; `square == 0` would divide by zero
            // one time in 2^106, which is still a division by zero.
            if square >= 1.0 || square == 0.0 {
                continue;
            }
            let factor = (-2.0 * square.ln() / square).sqrt();
            self.spare_normal = Some(y * factor);
            return x * factor;
        }
    }

    /// A normal deviate with the given mean and standard deviation.
    pub(super) fn normal_with(&mut self, mean: f64, sigma: f64) -> f64 {
        mean + sigma * self.normal()
    }

    /// A Poisson deviate — photon shot noise on `lambda` expected electrons.
    ///
    /// Returns `f64` rather than an integer because the caller immediately multiplies by a gain
    /// in ADU per electron; rounding here and again there would bias the result twice.
    ///
    /// See [`POISSON_EXACT_BELOW`] for the two-regime split. The approximation is clamped at
    /// zero: a Gaussian around a small mean can go negative, and a negative electron count
    /// propagates into a pixel value that underflows on the way to `u16`.
    pub(super) fn poisson(&mut self, lambda: f64) -> f64 {
        if lambda <= 0.0 {
            return 0.0;
        }
        if lambda < POISSON_EXACT_BELOW {
            // Knuth: multiply uniforms until the product falls below e^-λ; the number of factors
            // less one is the deviate.
            let limit = (-lambda).exp();
            let mut product = self.unit();
            let mut count = 0.0;
            while product > limit {
                product *= self.unit();
                count += 1.0;
            }
            return count;
        }
        self.normal_with(lambda, lambda.sqrt()).max(0.0)
    }

    /// A deviate from the stellar magnitude distribution over `bright..=faint`.
    ///
    /// Star counts rise as roughly `N(<m) ∝ 10^0.6m` — Pogson's slope for a uniform space
    /// density, which is close enough to the real counts over the six magnitudes a frame spans.
    /// Inverting it gives `m = faint + log10(u)/0.6` for `u` uniform, i.e. most stars sit within
    /// a magnitude of the limit and bright ones are rare, which is the property that makes a
    /// synthetic field look like a field rather than like confetti.
    pub(super) fn magnitude(&mut self, bright: f64, faint: f64) -> f64 {
        // `unit()` is half-open at 0, where the logarithm diverges; the clamp is what keeps a
        // one-in-2^53 draw from producing a star of magnitude −∞ (and a NaN flux).
        let u = self.unit().max(1e-12);
        (faint + u.log10() / 0.6).max(bright)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many draws the statistical tests use. Large enough that the assertions below have a
    /// wide margin, small enough to stay a unit test — and, because the stream is fixed, the
    /// margins are properties of *this* seed, not probabilities.
    const SAMPLES: usize = 100_000;

    fn mean_and_variance(values: &[f64]) -> (f64, f64) {
        let n = values.len() as f64;
        let mean = values.iter().sum::<f64>() / n;
        let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        (mean, variance)
    }

    #[test]
    fn the_same_seed_replays_the_same_stream() {
        // The whole contract. If this ever fails, every golden frame in the workspace has moved.
        let mut a = Rng::seeded(42);
        let mut b = Rng::seeded(42);
        let mut c = Rng::seeded(43);
        let first: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let second: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        let other: Vec<u64> = (0..8).map(|_| c.next_u64()).collect();
        assert_eq!(first, second);
        assert_ne!(first, other, "adjacent seeds must not correlate");
    }

    #[test]
    fn adjacent_stream_keys_are_unrelated() {
        // The catalogue seeds cells by consecutive indices, so this is the case that would go
        // wrong invisibly: correlated cells would make the sky repeat in a grid pattern.
        let mut first = Rng::stream(&[7, 0]);
        let mut second = Rng::stream(&[7, 1]);
        let a: Vec<u64> = (0..4).map(|_| first.next_u64()).collect();
        let b: Vec<u64> = (0..4).map(|_| second.next_u64()).collect();
        assert_ne!(a, b);
        // ...and the key is order-sensitive, or a (seed, cell) pair would collide with (cell,
        // seed) and two different skies would silently be one sky.
        let mut swapped = Rng::stream(&[0, 7]);
        assert_ne!(a[0], swapped.next_u64());
    }

    #[test]
    fn uniform_draws_stay_in_range_and_are_flat() {
        let mut rng = Rng::seeded(1);
        let values: Vec<f64> = (0..SAMPLES).map(|_| rng.unit()).collect();
        assert!(values.iter().all(|v| (0.0..1.0).contains(v)));
        let (mean, variance) = mean_and_variance(&values);
        assert!((mean - 0.5).abs() < 0.005, "mean {mean}");
        // A flat distribution on [0,1) has variance 1/12 = 0.0833.
        assert!((variance - 1.0 / 12.0).abs() < 0.002, "variance {variance}");
    }

    #[test]
    fn normal_draws_have_the_moments_read_noise_needs() {
        let mut rng = Rng::seeded(2);
        let values: Vec<f64> = (0..SAMPLES).map(|_| rng.normal_with(100.0, 5.0)).collect();
        let (mean, variance) = mean_and_variance(&values);
        assert!((mean - 100.0).abs() < 0.1, "mean {mean}");
        assert!(
            (variance.sqrt() - 5.0).abs() < 0.1,
            "sigma {}",
            variance.sqrt()
        );
    }

    #[test]
    fn poisson_variance_equals_its_mean_in_both_regimes() {
        // The defining property, and the one a wrong approximation breaks: shot noise that does
        // not scale as sqrt(signal) would make every SNR figure downstream meaningless.
        for lambda in [4.0_f64, 600.0] {
            let mut rng = Rng::seeded(3);
            let values: Vec<f64> = (0..SAMPLES).map(|_| rng.poisson(lambda)).collect();
            let (mean, variance) = mean_and_variance(&values);
            assert!(
                (mean - lambda).abs() < lambda * 0.02,
                "lambda {lambda}: mean {mean}"
            );
            assert!(
                (variance - lambda).abs() < lambda * 0.05,
                "lambda {lambda}: variance {variance}"
            );
            assert!(values.iter().all(|v| *v >= 0.0), "no negative electrons");
        }
        // Zero and negative expectations are the dark-frame case, not an error.
        let mut rng = Rng::seeded(4);
        assert_eq!(rng.poisson(0.0), 0.0);
        assert_eq!(rng.poisson(-1.0), 0.0);
    }

    #[test]
    fn magnitudes_thin_out_towards_the_bright_end() {
        let mut rng = Rng::seeded(5);
        let values: Vec<f64> = (0..SAMPLES).map(|_| rng.magnitude(-1.0, 18.0)).collect();
        assert!(values.iter().all(|m| (-1.0..=18.0).contains(m)));
        let brighter_than_16 = values.iter().filter(|m| **m < 16.0).count();
        let brighter_than_14 = values.iter().filter(|m| **m < 14.0).count();
        // Pogson's slope: `N(<m) ∝ 10^0.6m`, so two magnitudes fainter is 10^1.2 ≈ 15.8 times as
        // many stars. The window is wide because the denominator is small — only 0.4% of a
        // hundred thousand draws are brighter than 14 — so its own counting noise is a few
        // percent. Wide enough to be stable, narrow enough that a slope of 0.4 (ratio 6.3) or of
        // 0.8 (ratio 40) fails it, which is the mistake worth catching.
        let ratio = brighter_than_16 as f64 / brighter_than_14 as f64;
        assert!((12.0..20.0).contains(&ratio), "ratio {ratio}");
    }
}
