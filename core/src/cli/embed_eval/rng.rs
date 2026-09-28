//! A seeded pseudo-random generator for the eval's broken arms and
//! bootstrap. SplitMix64: small, fully specified, and the same stream on
//! every platform for a given seed, which is what makes a run reproducible
//! from the seed in `variants.toml` alone.

pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`, 53 bits of precision.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `0..n`. `n` must be non-zero.
    pub fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        // Lemire's multiply-shift; the bias for n far below 2^64 is negligible
        // for resampling and shuffling.
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }

    /// Standard normal, Box-Muller.
    pub fn gaussian(&mut self) -> f64 {
        let u1 = 1.0 - self.next_f64(); // (0, 1], so ln is finite
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    /// Fisher-Yates, in place.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }

    /// A permutation of `0..n` with no fixed point (Sattolo's algorithm,
    /// which yields a single n-cycle). For `n < 2` there is no derangement
    /// and the identity is returned.
    pub fn derangement(&mut self, n: usize) -> Vec<usize> {
        let mut p: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            let j = self.below(i);
            p.swap(i, j);
        }
        p
    }

    /// A unit vector of `dim` i.i.d. Gaussian components.
    pub fn unit_gaussian_vector(&mut self, dim: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dim).map(|_| self.gaussian() as f32).collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Control: making `next_u64` depend on anything but the seed (e.g.
    /// seeding from the clock) fails this.
    #[test]
    fn the_same_seed_gives_the_same_stream() {
        let a: Vec<u64> = {
            let mut r = Rng::new(398);
            (0..5).map(|_| r.next_u64()).collect()
        };
        let b: Vec<u64> = {
            let mut r = Rng::new(398);
            (0..5).map(|_| r.next_u64()).collect()
        };
        assert_eq!(a, b);
        assert_ne!(a, {
            let mut r = Rng::new(399);
            (0..5).map(|_| r.next_u64()).collect::<Vec<_>>()
        });
    }

    /// Control: replacing Sattolo's `below(i)` with Fisher-Yates' `below(i + 1)`
    /// admits fixed points, and some seed below finds one.
    #[test]
    fn a_derangement_moves_every_element() {
        for seed in 0..200 {
            let p = Rng::new(seed).derangement(7);
            let mut sorted = p.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..7).collect::<Vec<_>>(), "not a permutation: {p:?}");
            assert!(p.iter().enumerate().all(|(i, &j)| i != j), "fixed point in {p:?} (seed {seed})");
        }
    }

    /// Control: dropping the normalization in `unit_gaussian_vector` fails the
    /// norm check; a constant generator fails the mean/variance check.
    #[test]
    fn gaussian_vectors_are_unit_length_and_roughly_standard() {
        let mut r = Rng::new(1);
        let v = r.unit_gaussian_vector(384);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "{norm}");

        let samples: Vec<f64> = (0..20_000).map(|_| r.gaussian()).collect();
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        let var = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / samples.len() as f64;
        assert!(mean.abs() < 0.03, "{mean}");
        assert!((var - 1.0).abs() < 0.05, "{var}");
    }

    #[test]
    fn below_stays_in_range() {
        let mut r = Rng::new(7);
        for n in [1usize, 2, 3, 10, 1000] {
            for _ in 0..1000 {
                assert!(r.below(n) < n);
            }
        }
    }
}
