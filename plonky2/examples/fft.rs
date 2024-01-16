use std::time::Instant;

use plonky2_field::fft::fft;
use plonky2_field::goldilocks_field::GoldilocksField;
use plonky2_field::polynomial::{PolynomialCoeffs, PolynomialValues};
use plonky2_field::types::Field;

fn main() {
    type F = GoldilocksField;
    for log_degree in 12..=20 {
        let degree: usize = (1 << log_degree) - 50;
        let degree_padded = degree.next_power_of_two();

        let coeffs = (0..degree)
            .map(|i| F::from_canonical_usize(i * 1337 % 100))
            .chain(core::iter::repeat(F::ZERO).take(degree_padded - degree))
            .collect::<Vec<_>>();

        assert_eq!(coeffs.len(), degree_padded);
        let coefficients = PolynomialCoeffs { coeffs };
        let start = Instant::now();
        let points = fft(coefficients);
        let duration = start.elapsed();
        println!("Time elapsed in fft() with length 2^{:?} is: {:?}", log_degree, duration);
    }
}
